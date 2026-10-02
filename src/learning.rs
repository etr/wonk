//! Contrastive weight learning (TASK-102, DR-042/DR-043; closes OQ-019).
//!
//! The durable product of the feedback loop: recorded events become
//! bounded, decaying adjustments to ranking weights, keyed by FEATURE —
//! a bare signal name (`path_character`) or a flattened descriptive key
//! (`path:src/auth`, `symbol:kind=function`; TASK-105's flattening
//! contract). Signal names never contain `:` and no group name (`path`,
//! `symbol`, `match`, `graph`, `history`, `author`, `context`) collides
//! with a signal name, so the two sub-spaces are disjoint by construction
//! and one update rule covers both: signal keys learn an absolute weight
//! clamped multiplicatively around the configured default and overlay the
//! `WeightTable`; descriptive keys learn around default 0 within the same
//! scalar as an absolute bound and feed the `feedback` signal (D6).
//!
//! The rule (tuned in `bench/feedback-learning-tuning.md`): for one
//! qualifying event with useful member U and alternatives ALT (the
//! members returned but not reported useful — other useful members of
//! the same call are not alternatives),
//!
//! ```text
//! A(k)  = v_U(k) − mean_{x∈ALT}( v_x(k) )             ∈ [−1, 1]
//! next  = clamp( decayed(k) + step · A(k),  lo(k), hi(k) )
//! ```
//!
//! where `decayed` pulls the stored weight one half-life toward its
//! default per `learn_half_life_days` of age, and the bounds are a
//! deviation span around the configured default for signal keys —
//! multiplicative `[d·(1−dev), d·(1+dev)]` for the positive defaults,
//! mirrored around a negative default so the pair never inverts — and
//! ±dev absolute for descriptive keys. Bound semantics, deliberately: a
//! signal whose configured default is 0 is pinned at `[0, 0]` — learning
//! modulates criteria the configuration enabled, enabling a criterion
//! stays a human decision, and the descriptive channel is where new
//! criteria emerge from evidence alone. Decay applies at BOTH learn time
//! and read time, so interleaved updates are recency-weighted and an
//! untouched row shrinks toward its default in display and ranking;
//! observation counts never decay (they are evidence, not influence).
//! The clamp re-runs at load time against the CURRENT defaults, so
//! tightening `dev` or editing a default immediately re-bounds stored
//! values — the adversarial guarantee survives config edits.
//!
//! Whole-event skip rules (no observation counting either): the useful
//! result was already rank 1 (PRD-FB-REQ-009 — learn only where the
//! ranking was wrong), and slates with zero alternatives.
//!
//! Learning runs synchronously in the feedback dispatch (best-effort: a
//! failure warns, events stay recorded, the watermark stays put so the
//! next feedback call replays them). Each qualifying event updates
//! `(feature, '')` and, when it carries a query class, `(feature,
//! class)` — both, always (PRD-FB-REQ-008).

use std::collections::{BTreeMap, BTreeSet, HashMap};

use anyhow::Result;
use rusqlite::Connection;

use crate::config::FeedbackConfig;
use crate::feedback::{FeatureGroups, SlateMember};

/// Seconds per day — the unit `learn_half_life_days` is expressed
/// against.
const SECS_PER_DAY: f64 = 86_400.0;

/// The watermark row key: the highest `feedback_events.id` processed.
const WATERMARK_KEY: &str = "event_watermark";

/// Tuned learning parameters (D3/D4), built from `[feedback]` plus the
/// configured signal defaults.
#[derive(Debug, Clone)]
pub struct LearnParams {
    /// Per-event fraction of the contrastive advantage applied to each
    /// feature weight.
    pub step: f32,
    /// Maximum deviation from the configured default.
    pub max_deviation: f32,
    /// Age in days over which an unrefreshed weight halves its distance
    /// from the default.
    pub half_life_days: i64,
    /// Observations a row needs before it influences ranking.
    pub min_observations: i64,
    /// Distinct sessions a row needs before it influences ranking.
    pub min_sessions: i64,
    /// Configured `[rank.weights]` entries — the DEFAULT a signal key
    /// learns around (absent signals default 0.0).
    signal_defaults: BTreeMap<String, f32>,
}

impl LearnParams {
    /// Derive the parameters from the resolved `[feedback]` section and
    /// the configured weight map (config load has already validated the
    /// ranges).
    pub fn from_config(feedback: &FeedbackConfig, weights: &HashMap<String, f32>) -> Self {
        Self {
            step: feedback.learn_step,
            max_deviation: feedback.learn_max_deviation,
            half_life_days: feedback.learn_half_life_days,
            min_observations: feedback.learn_min_observations,
            min_sessions: feedback.learn_min_sessions,
            signal_defaults: weights.iter().map(|(k, v)| (k.clone(), *v)).collect(),
        }
    }

    /// The default weight a feature learns around: the configured signal
    /// weight for a bare signal name (absent = 0), 0.0 for descriptive
    /// keys — a descriptive property is not a criterion until learned.
    pub fn default_of(&self, feature: &str) -> f32 {
        self.signal_defaults.get(feature).copied().unwrap_or(0.0)
    }

    /// Whether `feature` is a descriptive (group-prefixed) key. Signal
    /// names never contain `:`, so this is the disjointness test itself.
    pub fn is_descriptive(&self, feature: &str) -> bool {
        feature.contains(':')
    }

    /// The clamp bounds: a deviation SPAN around the configured default
    /// for signal keys — `span = max_deviation * |default|`, so the pair
    /// is `(default − span, default + span)` and never inverts, whichever
    /// sign the configured default carries (negative demotion-style
    /// weights are legal config). For a positive default the span is
    /// exactly the documented multiplicative semantics
    /// `[d·(1−dev), d·(1+dev)]`; a zero-default signal is pinned at
    /// `[0, 0]`. Descriptive keys learn around default 0 within ±dev
    /// absolute.
    pub fn bounds(&self, feature: &str) -> (f32, f32) {
        if self.is_descriptive(feature) {
            (-self.max_deviation, self.max_deviation)
        } else {
            let default = self.default_of(feature);
            let span = self.max_deviation * default.abs();
            (default - span, default + span)
        }
    }
}

/// Flatten one member's feature groups to learnable keys (TASK-105's
/// documented contract): bare signal names; `path:<name>=<value>` for the
/// path scalars; `<group>:<name>=<value>` for every other group's
/// entries; bare `path:<ancestor>` presence keys for the ancestor
/// directories (and the shared `__overflow__` key). Sorted, duplicate
/// — free — the BTreeMap order the update path iterates in.
pub fn flatten_keys(groups: &FeatureGroups) -> Vec<String> {
    let mut keys: BTreeSet<String> = groups
        .signals
        .iter()
        .map(|signal| signal.signal.clone())
        .collect();
    for (name, value) in &groups.path {
        if crate::feedback::PATH_SCALARS.contains(&name.as_str()) {
            keys.insert(format!("path:{name}={value}"));
        } else {
            // Ancestor presence (and the shared `__overflow__` key).
            keys.insert(format!("path:{name}"));
        }
    }
    for (prefix, map) in [
        ("symbol", &groups.symbol),
        ("match", &groups.match_),
        ("graph", &groups.graph),
        ("history", &groups.history),
        ("author", &groups.author),
        ("context", &groups.context),
    ] {
        for (name, value) in map {
            keys.insert(format!("{prefix}:{name}={value}"));
        }
    }
    keys.into_iter().collect()
}

/// One feature's pending weight change for one (scope, feature): the
/// contrastive advantage this event assigns it.
#[derive(Debug, Clone, PartialEq)]
pub struct PendingUpdate {
    /// `''` (overall) or a query-class name.
    pub scope: String,
    pub feature: String,
    pub advantage: f32,
}

/// The pure event → updates step (D3). U is the event's useful member,
/// ALT the `chosen == false` members; every feature observable on U or
/// any ALT gets `A(k) = v_U(k) − mean_ALT v_x(k)` for the overall scope
/// and, when the event carries a class, the class scope too. Skipped
/// events (rank 1, no alternatives, no useful member) yield nothing — no
/// observation counting either.
pub fn event_updates(
    event: &crate::feedback::FeedbackEvent,
    _params: &LearnParams,
) -> Vec<PendingUpdate> {
    if event.chosen_rank == 1 {
        return Vec::new();
    }
    let members = &event.features.members;
    let Some(useful) = members
        .iter()
        .find(|member| member.chosen && member.identity == event.result_identity)
        .or_else(|| {
            members
                .iter()
                .find(|member| member.identity == event.result_identity)
        })
    else {
        return Vec::new();
    };
    let alternatives: Vec<&SlateMember> = members.iter().filter(|m| !m.chosen).collect();
    if alternatives.is_empty() {
        return Vec::new();
    }

    let useful_keys: BTreeSet<String> = flatten_keys(&useful.groups).into_iter().collect();
    let alternative_keys: Vec<BTreeSet<String>> = alternatives
        .iter()
        .map(|member| flatten_keys(&member.groups).into_iter().collect())
        .collect();

    // Observable keys: present on U or any alternative; a signal counts as
    // present when ANY member's recorded contributions carry it.
    let mut observable = useful_keys.clone();
    for keys in &alternative_keys {
        observable.extend(keys.iter().cloned());
    }
    for member in members {
        for signal in &member.groups.signals {
            observable.insert(signal.signal.clone());
        }
    }

    let mut updates = Vec::new();
    for key in observable {
        let v_useful = value_of(useful, &useful_keys, &key);
        let mut sum = 0.0f32;
        for (member, keys) in alternatives.iter().zip(&alternative_keys) {
            sum += value_of(member, keys, &key);
        }
        let advantage = v_useful - sum / alternatives.len() as f32;
        updates.push(PendingUpdate {
            scope: String::new(),
            feature: key.clone(),
            advantage,
        });
        if let Some(class) = &event.query_class
            && !class.is_empty()
        {
            updates.push(PendingUpdate {
                scope: class.clone(),
                feature: key,
                advantage,
            });
        }
    }
    updates
}

/// The decay factor: `0.5^(age_secs / 86400 / half_life_days)`, computed
/// in f64 and cast to f32 so the operation order is fixed. A future
/// `updated_at` decays nothing (factor 1.0).
pub fn decay_factor(updated_at: i64, now: i64, half_life_days: i64) -> f32 {
    let age_secs = (now - updated_at).max(0) as f64;
    0.5f64.powf(age_secs / SECS_PER_DAY / half_life_days as f64) as f32
}

/// One weight step: decay toward the default, apply the advantage,
/// clamp to the feature's bounds.
pub fn next_weight(
    stored: f32,
    updated_at: i64,
    now: i64,
    advantage: f32,
    feature: &str,
    params: &LearnParams,
) -> f32 {
    let default = params.default_of(feature);
    let decayed =
        default + decay_factor(updated_at, now, params.half_life_days) * (stored - default);
    let (lo, hi) = params.bounds(feature);
    (decayed + params.step * advantage).clamp(lo, hi)
}

/// The feature value of one member: a signal's recorded contribution
/// VALUE (0 when the member lacks it), 1.0/0.0 presence for descriptive
/// keys.
fn value_of(member: &SlateMember, keys: &BTreeSet<String>, key: &str) -> f32 {
    if key.contains(':') {
        if keys.contains(key) { 1.0 } else { 0.0 }
    } else {
        member
            .groups
            .signals
            .iter()
            .find(|signal| signal.signal == key)
            .map(|signal| signal.value)
            .unwrap_or(0.0)
    }
}

/// Apply one event's updates inside the caller's transaction: session
/// bookkeeping first (the INSERT OR IGNORE's affected-row count decides
/// whether `sessions` increments), then the upsert with decay computed
/// in Rust from the read row — the SQL stays dead simple and the math
/// stays testable. The three statements are `prepare_cached`, so a
/// replay run prepares each once per connection instead of once per
/// update.
fn apply_updates(
    tx: &Connection,
    params: &LearnParams,
    updates: &[PendingUpdate],
    session: Option<&str>,
    now: i64,
) -> Result<()> {
    // A NULL session (pre-session-ids events) counts as one distinct
    // unknown source under the empty-string key.
    let session_key = session.unwrap_or("");
    for update in updates {
        let session_inserted = {
            let mut stmt = tx.prepare_cached(
                "INSERT OR IGNORE INTO learned_weight_sessions \
                 (feature, query_class, session) VALUES (?1, ?2, ?3)",
            )?;
            stmt.execute(rusqlite::params![update.feature, update.scope, session_key])? as i64
        };
        let current: Option<(f32, i64, i64, i64)> = {
            let mut stmt = tx.prepare_cached(
                "SELECT weight, observations, sessions, updated_at \
                 FROM learned_weights WHERE feature = ?1 AND query_class = ?2",
            )?;
            stmt.query_row(rusqlite::params![update.feature, update.scope], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
            })
            .map(Some)
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                other => Err(other),
            })?
        };
        // A first observation starts from the default, fresh (no decay).
        let (stored, observations, sessions, updated_at) =
            current.unwrap_or_else(|| (params.default_of(&update.feature), 0, 0, now));
        let weight = next_weight(
            stored,
            updated_at,
            now,
            update.advantage,
            &update.feature,
            params,
        );
        let mut stmt = tx.prepare_cached(
            "INSERT INTO learned_weights \
             (feature, query_class, weight, observations, sessions, updated_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
             ON CONFLICT(feature, query_class) DO UPDATE SET \
                 weight = excluded.weight, \
                 observations = excluded.observations, \
                 sessions = excluded.sessions, \
                 updated_at = excluded.updated_at",
        )?;
        stmt.execute(rusqlite::params![
            update.feature,
            update.scope,
            weight,
            observations + 1,
            sessions + session_inserted,
            now
        ])?;
    }
    Ok(())
}

/// The learning watermark: the highest `feedback_events.id` processed.
/// Zero when nothing has been learned yet.
pub fn read_watermark(conn: &Connection) -> Result<i64> {
    let value: Option<String> = conn
        .query_row(
            "SELECT value FROM learned_meta WHERE key = ?1",
            [WATERMARK_KEY],
            |row| row.get(0),
        )
        .map(Some)
        .or_else(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => Ok(None),
            other => Err(other),
        })?;
    match value {
        None => Ok(0),
        Some(raw) => Ok(raw
            .parse::<i64>()
            .map_err(|e| anyhow::anyhow!("corrupt {WATERMARK_KEY} in learned_meta: {e}"))?),
    }
}

/// Persist the watermark inside the caller's transaction.
fn write_watermark(tx: &Connection, id: i64) -> Result<()> {
    tx.execute(
        "INSERT INTO learned_meta (key, value) VALUES (?1, ?2) \
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        rusqlite::params![WATERMARK_KEY, id.to_string()],
    )?;
    Ok(())
}

/// Learn from every event past the watermark (the feedback-dispatch
/// trigger, D3): load `id > watermark` in id order in bounded chunks
/// (SQL `LIMIT`), apply each chunk's updates in one bounded transaction,
/// and advance the watermark per chunk INSIDE this call — so replay
/// memory and transaction size stay capped no matter how large the
/// backlog, and a crash mid-learn leaves the watermark at the last
/// committed chunk with the next call resuming from there (best-effort
/// contract). Skip-rule events (rank 1, single-member slates) are
/// filtered in SQL before their feature JSON is ever loaded; every
/// qualifying event still contributes, and the learned end-state is
/// identical to a one-shot application of the same stream.
pub fn learn_pending(
    conn: &Connection,
    feedback: &FeedbackConfig,
    weights: &HashMap<String, f32>,
    now: i64,
) -> Result<()> {
    learn_pending_bounded(conn, feedback, weights, now, LEARN_CHUNK_EVENTS)
}

/// Events per learning chunk — bounds the replay's memory (each row
/// carries the full serialized slate) and its transaction size.
const LEARN_CHUNK_EVENTS: i64 = 200;

/// [`learn_pending`] with an explicit chunk size — the multi-chunk
/// equivalence seam (a backlog larger than `chunk` commits in several
/// chunks; the end-state must not notice).
fn learn_pending_bounded(
    conn: &Connection,
    feedback: &FeedbackConfig,
    weights: &HashMap<String, f32>,
    now: i64,
    chunk: i64,
) -> Result<()> {
    if !feedback.enabled {
        return Ok(());
    }
    crate::db::ensure_feedback_tables(conn)?;
    let params = LearnParams::from_config(feedback, weights);
    let mut watermark = read_watermark(conn)?;
    loop {
        let (frontier, events) = crate::feedback::load_learning_chunk(conn, watermark, chunk)?;
        if frontier <= watermark {
            return Ok(());
        }
        let tx = conn.unchecked_transaction()?;
        for event in &events {
            let updates = event_updates(event, &params);
            if updates.is_empty() {
                continue;
            }
            apply_updates(&tx, &params, &updates, event.session.as_deref(), now)?;
        }
        // Past EVERY id up to the frontier — skipped events included —
        // exactly the watermark an unchunked replay would write.
        write_watermark(&tx, frontier)?;
        tx.commit()?;
        watermark = frontier;
    }
}

/// One learned-weights row with its supporting evidence, effective value
/// computed at load time (decayed, re-clamped against the CURRENT
/// defaults). `gated` marks whether the row clears the
/// observation/session gates — inert rows are still evidence and stay
/// displayable (PRD-FB-REQ-029).
#[derive(Debug, Clone, PartialEq)]
pub struct FeedbackEvidence {
    pub feature: String,
    /// `''` = overall; otherwise a query-class name.
    pub query_class: String,
    /// The decayed-at-`loaded_at` weight, re-clamped to current bounds.
    pub effective: f32,
    /// The configured default the row learns around.
    pub default: f32,
    pub observations: i64,
    pub sessions: i64,
    pub updated_at: i64,
    pub gated: bool,
}

/// The gated learned rows loaded at one instant (D6): `load_learned`
/// filters to rows that clear both gates AND differ from their default,
/// so `None` means "ranking must behave exactly as feature-disabled"
/// (PRD-FB-REQ-020).
#[derive(Debug, Clone)]
pub struct LearnedTable {
    rows: Vec<FeedbackEvidence>,
    loaded_at: i64,
}

impl LearnedTable {
    /// Build a table from explicit rows — the in-crate test seam;
    /// `load_learned` is the production path.
    #[cfg(test)]
    pub(crate) fn from_rows(rows: Vec<FeedbackEvidence>, loaded_at: i64) -> Self {
        Self { rows, loaded_at }
    }

    /// Every gated row, all scopes, `(feature, scope)`-ordered.
    pub fn evidence(&self) -> &[FeedbackEvidence] {
        &self.rows
    }

    /// The instant the effective values were computed at — the
    /// descriptive pass's extraction clock.
    pub fn loaded_at(&self) -> i64 {
        self.loaded_at
    }

    /// Resolve per query class (D6): a class-scoped gated row wins over
    /// the overall row; a feature with neither contributes nothing and
    /// its default stands.
    pub fn resolve(&self, class: crate::rerank::QueryClass) -> ResolvedFeedback {
        let class_name = class.as_str();
        let mut resolved = ResolvedFeedback {
            loaded_at: self.loaded_at,
            ..ResolvedFeedback::default()
        };
        for pass in [class_name, ""] {
            for row in &self.rows {
                if row.query_class != pass
                    || resolved.signals.contains_key(&row.feature)
                    || resolved.descriptive.contains_key(&row.feature)
                {
                    continue;
                }
                if row.feature.contains(':') {
                    resolved
                        .descriptive
                        .insert(row.feature.clone(), row.effective);
                } else {
                    resolved.signals.insert(row.feature.clone(), row.effective);
                }
                resolved.evidence.push(row.clone());
            }
        }
        resolved
    }
}

/// The gated learned state one query resolved to: the signal overlay
/// (absolute weights replacing the class-multiplied defaults) and the
/// descriptive keys feeding the `feedback` signal, with the evidence
/// behind both (D6).
#[derive(Debug, Clone, Default)]
pub struct ResolvedFeedback {
    /// Bare signal names → learned absolute weights.
    pub signals: BTreeMap<String, f32>,
    /// Flattened descriptive keys → learned weights (summed per result
    /// into the `feedback` signal's value).
    pub descriptive: BTreeMap<String, f32>,
    /// The gated rows in effect for the resolved class, in
    /// `(feature, scope)` order.
    pub evidence: Vec<FeedbackEvidence>,
    /// The `load_learned` instant (the descriptive pass's clock).
    pub loaded_at: i64,
}

/// One raw `learned_weights` row as stored.
type RawRow = (String, String, f32, i64, i64, i64);

/// Read every stored row in `(feature, query_class)` order. `Ok(None)`
/// when the table is missing (pre-migration index) — silently, per the
/// best-effort contract.
fn read_raw_rows(conn: &Connection) -> Result<Option<Vec<RawRow>>> {
    let exists: i64 = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master \
         WHERE type = 'table' AND name = 'learned_weights')",
        [],
        |row| row.get(0),
    )?;
    if exists == 0 {
        return Ok(None);
    }
    let mut stmt = conn.prepare(
        "SELECT feature, query_class, weight, observations, sessions, updated_at \
         FROM learned_weights ORDER BY feature, query_class",
    )?;
    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<RawRow>>>()?;
    Ok(Some(rows))
}

/// The read-side view of one stored row: the decayed, re-clamped
/// effective value with its evidence and gate verdict.
fn evidence_of(raw: RawRow, params: &LearnParams, now: i64) -> FeedbackEvidence {
    let (feature, query_class, stored, observations, sessions, updated_at) = raw;
    let default = params.default_of(&feature);
    let (lo, hi) = params.bounds(&feature);
    let effective = (default
        + decay_factor(updated_at, now, params.half_life_days) * (stored - default))
        .clamp(lo, hi);
    let gated = observations >= params.min_observations
        && sessions >= params.min_sessions
        && effective != default;
    FeedbackEvidence {
        feature,
        query_class,
        effective,
        default,
        observations,
        sessions,
        updated_at,
        gated,
    }
}

/// Load the gated learned weights (D6): one SELECT over `learned_weights`,
/// effective values decayed at `now` and re-clamped against the CURRENT
/// defaults (the adversarial guarantee survives config edits). `Ok(None)`
/// — deliberately, not an error — when the table is missing
/// (pre-migration index), `[feedback]` is disabled, or nothing clears the
/// gates: all three mean "no influence" (PRD-FB-REQ-020/025/026).
pub fn load_learned(
    conn: &Connection,
    feedback: &FeedbackConfig,
    weights: &HashMap<String, f32>,
    now: i64,
) -> Result<Option<LearnedTable>> {
    if !feedback.enabled {
        return Ok(None);
    }
    let Some(raw) = read_raw_rows(conn)? else {
        return Ok(None);
    };
    let params = LearnParams::from_config(feedback, weights);
    let rows: Vec<FeedbackEvidence> = raw
        .into_iter()
        .map(|row| evidence_of(row, &params, now))
        .filter(|row| row.gated)
        .collect();
    if rows.is_empty() {
        return Ok(None);
    }
    Ok(Some(LearnedTable {
        rows,
        loaded_at: now,
    }))
}

/// List EVERY learned row — gated and inert, all scopes — decayed at
/// `now` and re-clamped: the `wonk feedback --weights` surface
/// (PRD-FB-REQ-029/012). Pure inspection: unlike [`load_learned`],
/// `[feedback] enabled` is not consulted (display, not influence), so
/// the evidence stays legible with the feature off.
pub fn list_learned(
    conn: &Connection,
    feedback: &FeedbackConfig,
    weights: &HashMap<String, f32>,
    now: i64,
) -> Result<Vec<FeedbackEvidence>> {
    let Some(raw) = read_raw_rows(conn)? else {
        return Ok(Vec::new());
    };
    let params = LearnParams::from_config(feedback, weights);
    Ok(raw
        .into_iter()
        .map(|row| evidence_of(row, &params, now))
        .collect())
}

/// The canonical repo-relative key of a result path as the search
/// produced it — the prepare's resolution, falling back to the raw
/// string stripped of a leading `./` (feedback.rs's `canonical_of`,
/// inlined against the context the pass holds).
fn canonical_key(ctx: &crate::rerank::SharedContext, as_seen: &std::path::Path) -> String {
    let raw = as_seen.to_string_lossy();
    match ctx.canonical_file(&raw) {
        Some(canonical) => canonical.to_string(),
        None => raw.strip_prefix("./").unwrap_or(&raw).to_string(),
    }
}

/// The descriptive application pass (D6, PRD-FB-REQ-014): one
/// `feedback` contribution row per candidate with
/// `value = clamp(Σ matched descriptive weights, −1, 1)`, joining the
/// score BEFORE the sort — features are rank-independent, so there is
/// none of novelty's circularity. Extraction mirrors the slate build
/// exactly (canonical files, bulk-loaded owning symbols, groups over
/// the shared context, cardinality cap included), so the keys a
/// candidate matches here are the keys the slate recorded there — and
/// it IS that extraction: the prepared bundle this pass returns (symbol
/// map + capped groups, `author_features` widened to the union of the
/// configuration's switch and the learned `author:` keys so neither
/// consumer's observable key set shrinks) rides the shared context for
/// the slate build to reuse — the heaviest per-query feedback work runs
/// once, not twice. `resolved.loaded_at` is the extraction clock
/// (determinism).
pub(crate) fn apply_feedback_contribution(
    scored: &mut [crate::rerank::ScoredResult],
    ctx: &crate::rerank::SharedContext,
    resolved: &ResolvedFeedback,
    query: &crate::rerank::QueryInfo<'_>,
    conn: &Connection,
    weight: f32,
    author_features: bool,
) -> Result<crate::feedback::FeedbackExtraction> {
    let canonicals: Vec<String> = scored
        .iter()
        .map(|item| canonical_key(ctx, &item.classified.result.file))
        .collect();
    let files: Vec<String> = canonicals
        .iter()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .cloned()
        .collect();
    let symbols = crate::feedback::load_symbols_by_file(conn, &files)?;
    let inputs = crate::feedback::ExtractionInputs {
        query: query.pattern,
        ctx,
        now: std::time::SystemTime::UNIX_EPOCH
            + std::time::Duration::from_secs(resolved.loaded_at.max(0) as u64),
        // Author data is extracted when the configuration records it OR
        // some `author:` key was learned: the pass's own contribution
        // never changes (an `author:` key can only match when one was
        // learned), and the slate build — reusing this extraction —
        // keeps every key its configuration records.
        author_features: author_features
            || resolved
                .descriptive
                .keys()
                .any(|key| key.starts_with("author:")),
    };

    let mut extracted = Vec::with_capacity(scored.len());
    for (item, canonical) in scored.iter().zip(&canonicals) {
        let rows = symbols.get(canonical).map(Vec::as_slice).unwrap_or(&[]);
        extracted.push(crate::feedback::extract_groups(
            item,
            crate::feedback::owning_symbol(rows, item.classified.result.line),
            canonical,
            &inputs,
        ));
    }
    crate::feedback::apply_cardinality_cap(&mut extracted);

    for (item, groups) in scored.iter_mut().zip(&extracted) {
        let mut value = 0.0f32;
        for key in flatten_keys(groups) {
            if let Some(learned) = resolved.descriptive.get(&key) {
                value += learned;
            }
        }
        let value = value.clamp(-1.0, 1.0);
        let weighted = value * weight;
        item.score += weighted;
        item.contributions.push(crate::rerank::Contribution {
            signal: "feedback",
            value,
            weight,
            weighted,
        });
    }

    // The prepared bundle for the slate build: capped groups keyed
    // (canonical file, line) — the shape `build_members` resolves by.
    let mut groups = HashMap::with_capacity(extracted.len());
    for ((canonical, item), member_groups) in canonicals.iter().zip(scored.iter()).zip(&extracted) {
        groups.insert(
            (canonical.clone(), item.classified.result.line),
            member_groups.clone(),
        );
    }
    Ok(crate::feedback::FeedbackExtraction { symbols, groups })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::feedback::{FeedbackEvent, SlateFeatures};

    fn contribution(signal: &str, value: f32) -> crate::output::ContributionOutput {
        crate::output::ContributionOutput {
            signal: signal.to_string(),
            value,
            weight: 1.0,
            weighted: value,
        }
    }

    fn groups_with_signals(signals: &[(&str, f32)]) -> FeatureGroups {
        FeatureGroups {
            signals: signals
                .iter()
                .map(|(signal, value)| contribution(signal, *value))
                .collect(),
            ..Default::default()
        }
    }

    fn member(rank: usize, chosen: bool, groups: FeatureGroups) -> SlateMember {
        SlateMember {
            identity: format!("id{rank}"),
            rank,
            chosen,
            file: format!("f{rank}.rs"),
            line: rank as u64,
            symbol: Some(format!("sym{rank}")),
            kind: Some("function".to_string()),
            score: 1.0 - rank as f32 * 0.1,
            groups,
        }
    }

    fn event_for(
        useful: &SlateMember,
        members: Vec<SlateMember>,
        class: Option<&str>,
    ) -> FeedbackEvent {
        FeedbackEvent {
            id: 1,
            result_identity: useful.identity.clone(),
            query_class: class.map(str::to_string),
            chosen_rank: useful.rank as i64,
            features: SlateFeatures {
                schema: 1,
                slate: "t".to_string(),
                members,
            },
            useful: true,
            session: Some("s1".to_string()),
            created_at: 0,
        }
    }

    fn default_params() -> LearnParams {
        LearnParams::from_config(
            &FeedbackConfig::default(),
            &HashMap::from([
                ("kind".to_string(), 1.0),
                ("path_character".to_string(), 0.6),
            ]),
        )
    }

    // -- flatten ------------------------------------------------------------

    #[test]
    fn flatten_forms_every_key_shape_of_the_contract() {
        let mut groups = FeatureGroups {
            signals: vec![
                contribution("kind", 1.0),
                contribution("path_character", 0.2),
            ],
            ..FeatureGroups::default()
        };
        groups.path.insert("src".to_string(), "1".to_string());
        groups.path.insert("src/auth".to_string(), "1".to_string());
        groups
            .path
            .insert("__overflow__".to_string(), "1".to_string());
        groups.path.insert("class".to_string(), "test".to_string());
        groups.path.insert("depth".to_string(), "mid".to_string());
        groups.path.insert("lang".to_string(), "Rust".to_string());
        groups
            .symbol
            .insert("kind".to_string(), "function".to_string());
        groups
            .symbol
            .insert("scoped".to_string(), "nested".to_string());
        groups
            .symbol
            .insert("name_match".to_string(), "exact".to_string());
        groups
            .symbol
            .insert("body_size".to_string(), "small".to_string());
        groups
            .match_
            .insert("category".to_string(), "definition".to_string());
        groups
            .match_
            .insert("term_coverage".to_string(), "most".to_string());
        groups
            .match_
            .insert("anchored".to_string(), "symbol".to_string());
        groups.graph.insert("hub".to_string(), "low".to_string());
        groups
            .graph
            .insert("authority".to_string(), "top".to_string());
        groups
            .graph
            .insert("fan_in".to_string(), "medium".to_string());
        groups
            .graph
            .insert("fan_out".to_string(), "zero".to_string());
        groups
            .graph
            .insert("community".to_string(), "7".to_string());
        groups
            .history
            .insert("recency".to_string(), "days".to_string());
        groups
            .history
            .insert("churn".to_string(), "high".to_string());
        groups
            .author
            .insert("last_touched_by".to_string(), "Ada".to_string());
        groups
            .author
            .insert("primary".to_string(), "Grace".to_string());
        groups
            .context
            .insert("same_file".to_string(), "yes".to_string());
        groups
            .context
            .insert("same_directory".to_string(), "no".to_string());
        groups
            .context
            .insert("same_community".to_string(), "yes".to_string());
        groups
            .context
            .insert("import_distance".to_string(), "direct".to_string());
        groups
            .context
            .insert("co_change".to_string(), "weak".to_string());

        let mut expect: Vec<String> = vec![
            "kind".into(),
            "path_character".into(),
            "path:__overflow__".into(),
            "path:src".into(),
            "path:src/auth".into(),
            "path:class=test".into(),
            "path:depth=mid".into(),
            "path:lang=Rust".into(),
            "symbol:kind=function".into(),
            "symbol:scoped=nested".into(),
            "symbol:name_match=exact".into(),
            "symbol:body_size=small".into(),
            "match:category=definition".into(),
            "match:term_coverage=most".into(),
            "match:anchored=symbol".into(),
            "graph:hub=low".into(),
            "graph:authority=top".into(),
            "graph:fan_in=medium".into(),
            "graph:fan_out=zero".into(),
            "graph:community=7".into(),
            "history:recency=days".into(),
            "history:churn=high".into(),
            "author:last_touched_by=Ada".into(),
            "author:primary=Grace".into(),
            "context:same_file=yes".into(),
            "context:same_directory=no".into(),
            "context:same_community=yes".into(),
            "context:import_distance=direct".into(),
            "context:co_change=weak".into(),
        ];
        expect.sort();
        assert_eq!(flatten_keys(&groups), expect);
    }

    #[test]
    fn descriptive_keys_and_signal_names_are_disjoint_by_construction() {
        let known = crate::rerank::known_signal_names();
        for name in &known {
            assert!(!name.contains(':'), "signal name carries a colon: {name}");
        }
        for group in [
            "path", "symbol", "match", "graph", "history", "author", "context",
        ] {
            assert!(
                !known.contains(&group),
                "group name collides with a signal name: {group}"
            );
        }
    }

    // -- advantage ------------------------------------------------------------

    #[test]
    fn advantage_is_useful_minus_mean_of_alternatives() {
        let useful = member(2, true, groups_with_signals(&[("path_character", 1.0)]));
        let a1 = member(1, false, groups_with_signals(&[("path_character", 0.2)]));
        let a2 = member(3, false, groups_with_signals(&[("path_character", 0.4)]));
        let event = event_for(&useful, vec![a1, useful.clone(), a2], Some("symbol"));
        let updates = event_updates(&event, &default_params());
        let pc: Vec<&PendingUpdate> = updates
            .iter()
            .filter(|update| update.feature == "path_character")
            .collect();
        assert_eq!(pc.len(), 2, "overall + class scope: {updates:?}");
        assert_eq!(pc[0].scope, "");
        assert_eq!(pc[1].scope, "symbol");
        // A = 1.0 − mean(0.2, 0.4) = 0.7 in BOTH scopes.
        assert!((pc[0].advantage - 0.7).abs() < 1e-6, "{:?}", pc[0]);
        assert!((pc[1].advantage - 0.7).abs() < 1e-6, "{:?}", pc[1]);
    }

    #[test]
    fn null_class_event_updates_overall_only() {
        let useful = member(2, true, groups_with_signals(&[("kind", 1.0)]));
        let alt = member(1, false, groups_with_signals(&[("kind", 0.0)]));
        let event = event_for(&useful, vec![alt, useful.clone()], None);
        let updates = event_updates(&event, &default_params());
        assert!(updates.iter().all(|update| update.scope.is_empty()));
    }

    #[test]
    fn other_useful_members_are_not_alternatives() {
        let u2 = member(2, true, groups_with_signals(&[("path_character", 1.0)]));
        let u3 = member(3, true, groups_with_signals(&[("path_character", 0.0)]));
        let a1 = member(1, false, groups_with_signals(&[("path_character", 0.2)]));
        let event = event_for(&u2, vec![a1, u2.clone(), u3], None);
        let updates = event_updates(&event, &default_params());
        let pc = updates
            .iter()
            .find(|update| update.feature == "path_character")
            .unwrap();
        // ALT is ONLY a1: A = 1.0 − 0.2.
        assert!((pc.advantage - 0.8).abs() < 1e-6, "{pc:?}");
    }

    #[test]
    fn absent_signal_counts_as_zero_on_alternatives() {
        let useful = member(2, true, groups_with_signals(&[("churn", 0.9)]));
        let alt = member(1, false, groups_with_signals(&[]));
        let event = event_for(&useful, vec![alt, useful.clone()], None);
        let updates = event_updates(&event, &default_params());
        let churn = updates
            .iter()
            .find(|update| update.feature == "churn")
            .unwrap();
        assert!((churn.advantage - 0.9).abs() < 1e-6, "{churn:?}");
    }

    #[test]
    fn descriptive_advantage_is_presence_fraction() {
        let mut useful_groups = FeatureGroups::default();
        useful_groups
            .path
            .insert("src/auth".to_string(), "1".to_string());
        let useful = member(2, true, useful_groups);
        let mut alt_groups = FeatureGroups::default();
        alt_groups
            .path
            .insert("src/auth".to_string(), "1".to_string());
        let a1 = member(1, false, alt_groups);
        let a2 = member(3, false, FeatureGroups::default());
        let a3 = member(4, false, FeatureGroups::default());
        let event = event_for(&useful, vec![a1, useful.clone(), a2, a3], None);
        let updates = event_updates(&event, &default_params());
        let key = updates
            .iter()
            .find(|update| update.feature == "path:src/auth")
            .unwrap();
        // v_U = 1, ALT = [1, 0, 0] → A = 1 − 1/3.
        assert!((key.advantage - (1.0 - 1.0 / 3.0)).abs() < 1e-6, "{key:?}");
    }

    #[test]
    fn unobservable_features_get_no_update() {
        let useful = member(2, true, groups_with_signals(&[("kind", 1.0)]));
        let alt = member(1, false, groups_with_signals(&[("kind", 0.8)]));
        let event = event_for(&useful, vec![alt, useful.clone()], None);
        let updates = event_updates(&event, &default_params());
        assert!(
            updates.iter().all(|update| update.feature != "lexical"),
            "signal on no member must not update: {updates:?}"
        );
        assert!(
            updates.iter().all(|update| update.feature != "path:src"),
            "key on no member must not update: {updates:?}"
        );
    }

    // -- skip rules ---------------------------------------------------------

    #[test]
    fn rank_one_event_is_skipped_entirely() {
        let useful = member(1, true, groups_with_signals(&[("kind", 1.0)]));
        let alt = member(2, false, groups_with_signals(&[("kind", 0.1)]));
        let mut event = event_for(&useful, vec![useful.clone(), alt], None);
        event.chosen_rank = 1;
        assert!(event_updates(&event, &default_params()).is_empty());
    }

    #[test]
    fn event_without_alternatives_is_skipped() {
        let useful = member(1, true, groups_with_signals(&[("kind", 1.0)]));
        let event = event_for(&useful, vec![useful.clone()], None);
        assert!(event_updates(&event, &default_params()).is_empty());
    }

    // -- update math ---------------------------------------------------------

    #[test]
    fn next_weight_decays_then_steps_then_clamps() {
        let params = default_params();
        // Fresh row at the default: no decay, one step of the advantage.
        let next = next_weight(0.6, 1000, 1000, 0.8, "path_character", &params);
        assert!((next - (0.6 + 0.02 * 0.8)).abs() < 1e-6, "{next}");
        // One half-life old, no advantage: halfway back to the default.
        let next = next_weight(0.9, 0, 30 * 86_400, 0.0, "path_character", &params);
        assert!((next - 0.75).abs() < 1e-6, "{next}");
        // Clamp at the (computed) bounds.
        let (lo, hi) = params.bounds("path_character");
        assert_eq!(next_weight(0.6, 0, 0, 100.0, "path_character", &params), hi);
        assert_eq!(
            next_weight(0.6, 0, 0, -100.0, "path_character", &params),
            lo
        );
    }

    #[test]
    fn zero_default_signal_is_pinned_at_zero() {
        let params = default_params();
        // churn is absent from the defaults map → default 0 → bounds [0, 0].
        assert_eq!(next_weight(0.0, 0, 0, 1.0, "churn", &params), 0.0);
        // Even a stored nonzero value re-clamps to 0.
        assert_eq!(next_weight(0.5, 0, 0, 0.5, "churn", &params), 0.0);
    }

    #[test]
    fn deviation_one_floors_signal_weights_at_zero() {
        let params = LearnParams::from_config(
            &FeedbackConfig {
                learn_max_deviation: 1.0,
                ..FeedbackConfig::default()
            },
            &HashMap::from([("kind".to_string(), 0.4)]),
        );
        assert_eq!(next_weight(0.4, 0, 0, -1_000.0, "kind", &params), 0.0);
    }

    #[test]
    fn negative_default_bounds_do_not_invert() {
        // A demotion-style negative configured weight is legal config
        // (WeightTable::from_config rejects only unknown names and
        // non-finite values); its bounds must stay ordered.
        let params = LearnParams::from_config(
            &FeedbackConfig::default(),
            &HashMap::from([("path_character".to_string(), -0.2)]),
        );
        // span = dev * |default| = 0.5 * 0.2 → (-0.3, -0.1).
        let (lo, hi) = params.bounds("path_character");
        assert!(
            lo < hi,
            "bounds must not invert for a negative default: ({lo}, {hi})"
        );
        assert!((lo - -0.3).abs() < 1e-6, "lo: {lo}");
        assert!((hi - -0.1).abs() < 1e-6, "hi: {hi}");
        // next_weight clamps to a bounded value instead of panicking.
        assert_eq!(
            next_weight(-0.2, 1000, 1000, 1_000.0, "path_character", &params),
            hi,
            "a saturating positive advantage stops at the upper bound"
        );
        assert_eq!(
            next_weight(-0.2, 1000, 1000, -1_000.0, "path_character", &params),
            lo,
            "a saturating negative advantage stops at the lower bound"
        );
    }

    #[test]
    fn negative_default_learn_and_load_stay_bounded() {
        // The persistence round trip over a negative default: learn one
        // event, then read the row back through evidence_of's load-time
        // re-clamp — both bounded, neither panicking.
        let weights = HashMap::from([("path_character".to_string(), -0.2)]);
        let params = LearnParams::from_config(&enabled_config(), &weights);
        let (lo, hi) = params.bounds("path_character");
        let conn = learning_conn();
        insert_event(&conn, 1, None, "s1", 1.0, 0.2);
        learn_pending(&conn, &enabled_config(), &weights, 1000).unwrap();
        let rows = learned_rows(&conn);
        let row = rows
            .iter()
            .find(|row| row.0 == "path_character" && row.1.is_empty())
            .expect("the negative-default signal learned");
        assert!(
            row.2 >= lo - 1e-6 && row.2 <= hi + 1e-6,
            "stored weight {} within ({lo}, {hi})",
            row.2
        );
        // The query path: every stored row re-clamps through evidence_of.
        let listed = list_learned(&conn, &enabled_config(), &weights, 1000).unwrap();
        for row in &listed {
            let (lo, hi) = params.bounds(&row.feature);
            assert!(
                row.effective >= lo - 1e-6 && row.effective <= hi + 1e-6,
                "{} effective {} outside ({lo}, {hi})",
                row.feature,
                row.effective
            );
        }
    }

    #[test]
    fn descriptive_keys_are_bounded_plus_minus_deviation() {
        let params = default_params();
        // Saturating advantages hit the ±dev bounds exactly.
        assert_eq!(next_weight(0.0, 0, 0, 100.0, "path:src", &params), 0.5);
        assert_eq!(next_weight(0.0, 0, 0, -100.0, "path:src", &params), -0.5);
        // One step lands inside, not at, the bound.
        let one = next_weight(0.0, 0, 0, 0.8, "path:src", &params);
        assert!((one - 0.016).abs() < 1e-6, "{one}");
    }

    #[test]
    fn decay_factor_halves_per_half_life_and_never_amplifies() {
        assert!((decay_factor(0, 30 * 86_400, 30) - 0.5).abs() < 1e-6);
        let ten = 0.5f64.powf(10.0) as f32;
        assert!((decay_factor(0, 300 * 86_400, 30) - ten).abs() < 1e-7);
        assert_eq!(decay_factor(500, 100, 30), 1.0, "future updated_at");
    }

    // -- persistence (Step 3) --------------------------------------------------
    //
    // In-memory rusqlite with the real `ensure_feedback_tables`, events
    // seeded straight into `feedback_events` the way `record_feedback`
    // writes them.

    use rusqlite::Connection;

    fn learning_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        crate::db::ensure_feedback_tables(&conn).unwrap();
        conn
    }

    /// Seed one two-member event — useful at rank 2 with `path_character`
    /// at `useful_value`, alternative at rank 1 with `alt_value`.
    fn insert_event(
        conn: &Connection,
        id: i64,
        class: Option<&str>,
        session: &str,
        useful_value: f32,
        alt_value: f32,
    ) {
        let useful = member(
            2,
            true,
            groups_with_signals(&[("path_character", useful_value)]),
        );
        let identity = useful.identity.clone();
        let alt = member(
            1,
            false,
            groups_with_signals(&[("path_character", alt_value)]),
        );
        let features = SlateFeatures {
            schema: 1,
            slate: format!("t{id}"),
            members: vec![alt, useful],
        };
        conn.execute(
            "INSERT INTO feedback_events \
             (id, result_identity, query_class, chosen_rank, features, useful, session, created_at) \
             VALUES (?1, ?2, ?3, 2, ?4, 1, ?5, ?6)",
            rusqlite::params![
                id,
                identity,
                class,
                serde_json::to_string(&features).unwrap(),
                session,
                id * 1000
            ],
        )
        .unwrap();
    }

    fn learned_rows(conn: &Connection) -> Vec<(String, String, f32, i64, i64, i64)> {
        let mut stmt = conn
            .prepare(
                "SELECT feature, query_class, weight, observations, sessions, updated_at \
                 FROM learned_weights ORDER BY feature, query_class",
            )
            .unwrap();
        stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, f32>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, i64>(5)?,
            ))
        })
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap()
    }

    fn default_weights() -> HashMap<String, f32> {
        HashMap::from([("path_character".to_string(), 0.6)])
    }

    /// Learning only runs with the feedback feature on (the dispatch
    /// shape).
    fn enabled_config() -> FeedbackConfig {
        FeedbackConfig {
            enabled: true,
            ..FeedbackConfig::default()
        }
    }

    #[test]
    fn learn_pending_updates_both_scopes_and_advances_watermark() {
        let conn = learning_conn();
        insert_event(&conn, 1, Some("symbol"), "s1", 1.0, 0.2);
        learn_pending(&conn, &enabled_config(), &default_weights(), 1000).unwrap();

        let rows = learned_rows(&conn);
        assert_eq!(rows.len(), 2, "overall + class scope: {rows:?}");
        assert_eq!(rows[0].1, "", "sorted: overall first");
        assert_eq!(rows[1].1, "symbol");
        for row in &rows {
            assert_eq!(row.0, "path_character");
            assert_eq!(row.3, 1, "one observation");
            assert_eq!(row.4, 1, "one session");
            assert_eq!(row.5, 1000, "updated_at = now");
        }
        assert_eq!(
            read_watermark(&conn).unwrap(),
            1,
            "watermark = highest processed id"
        );
        // A = 1.0 − 0.2 = 0.8 → 0.6 + 0.02·0.8.
        let expect = 0.6 + 0.02 * 0.8;
        assert!((rows[0].2 - expect).abs() < 1e-6, "{}", rows[0].2);
    }

    #[test]
    fn learn_pending_counts_interleaved_sessions_exactly() {
        let conn = learning_conn();
        // A, B, A across three events → 3 observations, 2 distinct
        // sessions (PRD-FB-REQ-025's exactness under interleaving).
        insert_event(&conn, 1, None, "A", 1.0, 0.2);
        insert_event(&conn, 2, None, "B", 1.0, 0.2);
        insert_event(&conn, 3, None, "A", 1.0, 0.2);
        learn_pending(&conn, &enabled_config(), &default_weights(), 1000).unwrap();

        let rows = learned_rows(&conn);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].3, 3, "observations");
        assert_eq!(rows[0].4, 2, "distinct sessions");
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM learned_weight_sessions", [], |row| {
                row.get::<_, i64>(0)
            },)
                .unwrap(),
            2
        );
    }

    #[test]
    fn learn_pending_processes_only_past_the_watermark() {
        let conn = learning_conn();
        insert_event(&conn, 1, None, "s1", 1.0, 0.2);
        insert_event(&conn, 2, None, "s1", 1.0, 0.2);
        learn_pending(&conn, &enabled_config(), &default_weights(), 1000).unwrap();

        // New event after the watermark: only it is processed this round.
        insert_event(&conn, 3, None, "s2", 1.0, 0.2);
        learn_pending(&conn, &enabled_config(), &default_weights(), 2000).unwrap();
        let rows = learned_rows(&conn);
        assert_eq!(rows[0].3, 3, "observations grow by exactly 1");
        assert_eq!(rows[0].4, 2);
        assert_eq!(rows[0].5, 2000, "updated_at = the latest now");

        // No new events: a replay is a no-op (watermark already past).
        learn_pending(&conn, &enabled_config(), &default_weights(), 3000).unwrap();
        let after = learned_rows(&conn);
        assert_eq!(after, rows, "idempotent under replay");
        assert_eq!(read_watermark(&conn).unwrap(), 3);
    }

    #[test]
    fn learn_pending_skipped_events_advance_the_watermark_without_rows() {
        let conn = learning_conn();
        insert_event(&conn, 1, None, "s1", 1.0, 0.2);
        // A rank-1 event: skipped whole (no observation counting)...
        let useful = member(1, true, groups_with_signals(&[("path_character", 1.0)]));
        let alt = member(2, false, groups_with_signals(&[("path_character", 0.1)]));
        let features = SlateFeatures {
            schema: 1,
            slate: "t2".to_string(),
            members: vec![useful, alt],
        };
        conn.execute(
            "INSERT INTO feedback_events \
             (id, result_identity, query_class, chosen_rank, features, useful, session, created_at) \
             VALUES (2, ?1, NULL, 1, ?2, 1, 's1', 0)",
            rusqlite::params![
                features.members[0].identity,
                serde_json::to_string(&features).unwrap()
            ],
        )
        .unwrap();
        learn_pending(&conn, &enabled_config(), &default_weights(), 1000).unwrap();

        let rows = learned_rows(&conn);
        assert_eq!(rows.len(), 1, "only the rank-2 event learned");
        assert_eq!(rows[0].3, 1);
        assert_eq!(
            read_watermark(&conn).unwrap(),
            2,
            "skipped events still advance the watermark"
        );
    }

    #[test]
    fn learn_pending_is_deterministic_under_replay() {
        let build = || {
            let conn = learning_conn();
            insert_event(&conn, 1, Some("symbol"), "A", 1.0, 0.2);
            insert_event(&conn, 2, Some("symbol"), "B", 0.9, 0.3);
            insert_event(&conn, 3, None, "A", 0.4, 0.6);
            learn_pending(&conn, &enabled_config(), &default_weights(), 1000).unwrap();
            conn
        };
        // Bit-equal weights: same events + same now → identical table.
        assert_eq!(learned_rows(&build()), learned_rows(&build()));
    }

    #[test]
    fn skip_rule_events_are_filtered_before_their_features_are_parsed() {
        let conn = learning_conn();
        insert_event(&conn, 1, None, "s1", 1.0, 0.2);
        // A rank-1 (skip-rule) event whose features payload is corrupt:
        // the replay must filter it in SQL — never parsing its JSON —
        // learn from the qualifying event, and still advance the
        // watermark past it.
        conn.execute(
            "INSERT INTO feedback_events \
             (id, result_identity, query_class, chosen_rank, features, useful, session, created_at) \
             VALUES (2, 'x', NULL, 1, 'not json', 1, 's1', 0)",
            [],
        )
        .unwrap();
        learn_pending(&conn, &enabled_config(), &default_weights(), 1000).unwrap();
        assert_eq!(learned_rows(&conn).len(), 1, "the qualifying event learned");
        assert_eq!(
            read_watermark(&conn).unwrap(),
            2,
            "the skipped event still advances the watermark"
        );
    }

    #[test]
    fn chunked_replay_reaches_the_one_shot_end_state() {
        let session_rows = |conn: &Connection| {
            conn.query_row("SELECT COUNT(*) FROM learned_weight_sessions", [], |row| {
                row.get::<_, i64>(0)
            })
            .unwrap()
        };
        let build = |chunk: i64| {
            let conn = learning_conn();
            // A backlog with every shape the chunk boundaries and the SQL
            // skip filters must handle identically: 25 qualifying events
            // across 3 interleaved sessions, one rank-1 skip event, and
            // one single-member-slate skip event.
            for id in 1..=25 {
                insert_event(&conn, id, None, &format!("s{}", id % 3), 1.0, 0.2);
            }
            let useful = member(1, true, groups_with_signals(&[("path_character", 1.0)]));
            let identity = useful.identity.clone();
            conn.execute(
                "INSERT INTO feedback_events \
                 (id, result_identity, query_class, chosen_rank, features, useful, session, created_at) \
                 VALUES (26, ?1, NULL, 2, ?2, 1, 's1', 0)",
                rusqlite::params![
                    identity,
                    serde_json::to_string(&SlateFeatures {
                        schema: 1,
                        slate: "solo".to_string(),
                        members: vec![useful],
                    })
                    .unwrap()
                ],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO feedback_events \
                 (id, result_identity, query_class, chosen_rank, features, useful, session, created_at) \
                 VALUES (27, 'x', NULL, 1, ?1, 1, 's1', 0)",
                rusqlite::params![serde_json::to_string(&SlateFeatures {
                    schema: 1,
                    slate: "rank1".to_string(),
                    members: vec![
                        member(1, true, groups_with_signals(&[("path_character", 1.0)])),
                        member(2, false, groups_with_signals(&[("path_character", 0.1)])),
                    ],
                })
                .unwrap()],
            )
            .unwrap();
            learn_pending_bounded(&conn, &enabled_config(), &default_weights(), 5000, chunk)
                .unwrap();
            conn
        };
        // chunk = 2 over 27 events → 14 committed chunks; chunk = 1_000
        // is the one-shot shape.
        let one_shot = build(1_000);
        let chunked = build(2);
        assert_eq!(
            learned_rows(&one_shot),
            learned_rows(&chunked),
            "identical learned end-state across chunk boundaries"
        );
        assert_eq!(
            session_rows(&one_shot),
            session_rows(&chunked),
            "identical session bookkeeping"
        );
        assert_eq!(read_watermark(&one_shot).unwrap(), 27);
        assert_eq!(
            read_watermark(&chunked).unwrap(),
            27,
            "skip events ride the frontier in every chunking"
        );
    }

    #[test]
    fn load_learned_gates_on_observations_and_sessions() {
        let conn = learning_conn();
        // One event / one session: below both gates → no influence at all.
        insert_event(&conn, 1, None, "s1", 1.0, 0.2);
        learn_pending(&conn, &enabled_config(), &default_weights(), 1000).unwrap();
        assert!(
            load_learned(&conn, &enabled_config(), &default_weights(), 1000)
                .unwrap()
                .is_none(),
            "below-gate rows must not surface"
        );

        // Ten events across three sessions: past both gates.
        let conn = learning_conn();
        for id in 1..=10 {
            insert_event(&conn, id, None, &format!("s{}", id % 3), 1.0, 0.2);
        }
        learn_pending(&conn, &enabled_config(), &default_weights(), 1000).unwrap();
        let table = load_learned(&conn, &enabled_config(), &default_weights(), 1000).unwrap();
        assert!(table.is_some(), "10 obs / 3 sessions clears the gates");
    }

    #[test]
    fn load_learned_decays_effective_weights_with_now() {
        let conn = learning_conn();
        for id in 1..=10 {
            insert_event(&conn, id, None, &format!("s{}", id % 4), 1.0, 0.2);
        }
        learn_pending(&conn, &enabled_config(), &default_weights(), 1000).unwrap();

        let table = load_learned(&conn, &enabled_config(), &default_weights(), 1000).unwrap();
        let fresh = table.unwrap();
        let (_, effective_fresh) = fresh_row(&fresh);
        let (_, effective_aged) = fresh_row(
            &load_learned(
                &conn,
                &enabled_config(),
                &default_weights(),
                1000 + 30 * 86_400,
            )
            .unwrap()
            .unwrap(),
        );
        // One half-life: the deviation from the default halves.
        let deviation_fresh = (effective_fresh - 0.6).abs();
        let deviation_aged = (effective_aged - 0.6).abs();
        assert!(
            (deviation_aged - deviation_fresh / 2.0).abs() < 1e-6,
            "{deviation_aged} vs {deviation_fresh}/2"
        );
        // Ten half-lives: effectively back at the default.
        let (_, effective_stale) = fresh_row(
            &load_learned(
                &conn,
                &enabled_config(),
                &default_weights(),
                1000 + 300 * 86_400,
            )
            .unwrap()
            .unwrap(),
        );
        assert!(
            (effective_stale - 0.6).abs() < 0.5 / 1000.0,
            "{effective_stale} must collapse to the default"
        );
    }

    fn fresh_row(table: &LearnedTable) -> (String, f32) {
        let evidence = table.evidence();
        let row = evidence
            .iter()
            .find(|row| row.feature == "path_character" && row.query_class.is_empty())
            .unwrap();
        (row.feature.clone(), row.effective)
    }

    #[test]
    fn load_learned_missing_table_is_silently_absent() {
        let conn = Connection::open_in_memory().unwrap();
        // A pre-TASK-102 index: no learned tables at all.
        assert!(
            load_learned(&conn, &enabled_config(), &default_weights(), 0)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn load_learned_reclamps_against_current_defaults() {
        let conn = learning_conn();
        for id in 1..=20 {
            insert_event(&conn, id, None, &format!("s{}", id % 4), 1.0, 0.2);
        }
        learn_pending(&conn, &enabled_config(), &default_weights(), 1000).unwrap();
        // The config tightens dev 0.5 → 0.1 after the weights were
        // learned: loading re-clamps to the new bounds (AR-043).
        let tightened = FeedbackConfig {
            learn_max_deviation: 0.1,
            ..enabled_config()
        };
        let table = load_learned(&conn, &tightened, &default_weights(), 1000)
            .unwrap()
            .unwrap();
        let (_, effective) = fresh_row(&table);
        assert!(
            (effective - 0.6).abs() <= 0.6 * 0.1 + 1e-6,
            "tightened bound must hold: {effective}"
        );
    }

    #[test]
    fn resolve_prefers_class_scope_then_overall_then_nothing() {
        let conn = learning_conn();
        // Enough mass to clear gates for both scopes.
        for id in 1..=12 {
            insert_event(&conn, id, Some("symbol"), &format!("s{}", id % 4), 1.0, 0.2);
        }
        learn_pending(&conn, &enabled_config(), &default_weights(), 1000).unwrap();
        let table = load_learned(&conn, &enabled_config(), &default_weights(), 1000).unwrap();

        let resolved = table
            .as_ref()
            .unwrap()
            .resolve(crate::rerank::QueryClass::Symbol);
        let class_row = resolved
            .evidence
            .iter()
            .find(|row| row.feature == "path_character")
            .unwrap();
        assert_eq!(class_row.query_class, "symbol", "class scope wins");

        let overall = table
            .as_ref()
            .unwrap()
            .resolve(crate::rerank::QueryClass::Path);
        let overall_row = overall
            .evidence
            .iter()
            .find(|row| row.feature == "path_character")
            .unwrap();
        assert_eq!(overall_row.query_class, "", "no class rows → overall");

        assert_eq!(
            resolved.signals.get("path_character"),
            overall.signals.get("path_character"),
            "same advantage in both scopes → same effective weight"
        );
        assert!(resolved.descriptive.is_empty());
        assert_eq!(resolved.loaded_at, 1000);
    }

    // -- the descriptive application pass (Step 4) -----------------------------

    use crate::ranker::ClassifiedResult;

    fn scored_of(file: &str, line: u64, content: &str) -> crate::rerank::ScoredResult {
        crate::rerank::ScoredResult {
            classified: ClassifiedResult {
                result: crate::search::SearchResult {
                    file: std::path::PathBuf::from(file),
                    line,
                    col: 1,
                    content: content.to_string(),
                },
                category: crate::ranker::ResultCategory::Other,
                annotation: None,
            },
            score: 0.0,
            contributions: Vec::new(),
        }
    }

    fn symbols_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE symbols (\
                 id INTEGER PRIMARY KEY, file TEXT, line INTEGER, end_line INTEGER, \
                 name TEXT, kind TEXT, scope TEXT, signature TEXT, language TEXT\
             );",
        )
        .unwrap();
        conn
    }

    #[test]
    fn apply_feedback_contribution_appends_one_row_per_candidate() {
        let conn = symbols_conn();
        let mut scored = vec![
            scored_of("src/auth/tokens.rs", 1, "fn mint"),
            scored_of("tools/x.rs", 1, "fn mint"),
        ];
        let resolved = ResolvedFeedback {
            descriptive: BTreeMap::from([("path:src/auth".to_string(), 0.6)]),
            loaded_at: 123_456,
            ..ResolvedFeedback::default()
        };
        let prepared = apply_feedback_contribution(
            &mut scored,
            &crate::rerank::SharedContext::default(),
            &resolved,
            &crate::rerank::QueryInfo { pattern: "mint" },
            &conn,
            0.5,
            false,
        )
        .unwrap();

        assert_eq!(scored[0].contributions.len(), 1);
        let feedback = &scored[0].contributions[0];
        assert_eq!(feedback.signal, "feedback");
        assert_eq!(feedback.value, 0.6, "sum of matched descriptive keys");
        assert_eq!(feedback.weight, 0.5);
        assert_eq!(feedback.weighted, 0.3);
        assert_eq!(scored[0].score, 0.3, "the weighted value joins the score");

        assert_eq!(scored[1].contributions.len(), 1, "a row per candidate");
        assert_eq!(scored[1].contributions[0].signal, "feedback");
        assert_eq!(scored[1].contributions[0].value, 0.0);
        assert_eq!(scored[1].score, 0.0);

        // The prepared bundle the slate build reuses: one capped group
        // per candidate, keyed (canonical, line), symbols for the file
        // set — the ONE extraction per query.
        assert_eq!(prepared.groups.len(), 2, "a group per candidate");
        assert!(
            prepared
                .groups
                .contains_key(&("src/auth/tokens.rs".to_string(), 1)),
            "keyed by canonical file and line"
        );
        // The fixture's symbols table is empty: the bulk-load map stays
        // empty while the groups still extract (line-anchored members).
        assert!(prepared.symbols.is_empty());
        assert!(
            prepared
                .groups
                .values()
                .all(|groups| groups.signals.is_empty())
        );
    }

    #[test]
    fn apply_feedback_contribution_clamps_the_matched_sum() {
        let conn = symbols_conn();
        let mut scored = vec![scored_of("src/auth/tokens.rs", 1, "fn mint")];
        let resolved = ResolvedFeedback {
            descriptive: BTreeMap::from([
                ("path:src".to_string(), 0.9),
                ("path:src/auth".to_string(), 0.9),
            ]),
            loaded_at: 0,
            ..ResolvedFeedback::default()
        };
        apply_feedback_contribution(
            &mut scored,
            &crate::rerank::SharedContext::default(),
            &resolved,
            &crate::rerank::QueryInfo { pattern: "mint" },
            &conn,
            1.0,
            false,
        )
        .unwrap();
        assert_eq!(
            scored[0].contributions[0].value, 1.0,
            "the sum clamps to [-1, 1]"
        );
        assert_eq!(scored[0].score, 1.0);
    }
}
