//! Usage-feedback capture (TASK-101, DR-042, PRD-FB-REQ-001..015).
//!
//! Wonk persists the ranked slate at SEARCH time (`feedback_slates`), and
//! the feedback call (`wonk_feedback` MCP tool / `wonk feedback` CLI)
//! references that persisted slate by token — so the recorded signal
//! vectors are wonk's own retained contributions, never caller-echoed and
//! never re-derived from a drifted re-search. Entries are keyed on a
//! content-anchored result identity (the review `finding_identity`
//! technique, TASK-085) that survives re-indexing; retirement is read-time
//! liveness resolution, never a write. Everything stays in the per-repo
//! index DB; there is no telemetry path (PRD-FB-REQ-004).

use std::collections::{BTreeMap, HashMap};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Result, bail};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Stable result identity: SHA-256 hex of
/// `"result" \x1f file \x1f kind \x1f name \x1f fold(signature)`.
///
/// The sibling of review's `finding_identity` (TASK-085) — component-joined
/// SHA-256 with a whitespace-folded text anchor and the line number
/// structurally absent, so line shifts cannot change identity. All
/// components come from one `symbols` row, which is what makes
/// re-derivation at read time possible (`live_identities`): the anchor is
/// the symbol's stored `signature`, not the matched line. A rename, kind
/// change, signature-token change, or file move yields a different
/// identity — the material-change boundary `ChangeAnalysisDetail.
/// signature_changed` already draws (PRD-FB-REQ-005/006).
pub fn result_identity(file: &str, kind: &str, name: &str, signature: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"result");
    for part in [file, kind, name] {
        hasher.update([0x1f]);
        hasher.update(part.as_bytes());
    }
    hasher.update([0x1f]);
    hasher.update(crate::review::fold_whitespace(signature).as_bytes());
    hex(hasher)
}

/// Stable identity for a result with no owning symbol: SHA-256 hex of
/// `"result-line" \x1f file \x1f category \x1f fold(content)`.
///
/// File-level matches (a line outside every symbol span) anchor on the
/// matched line's content instead of a signature; `category` is the
/// result's ranker category. These identities never retire by drift —
/// they carry `symbol: null` so the learner can down-weight them.
pub fn line_identity(file: &str, category: &str, content: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"result-line");
    for part in [file, category] {
        hasher.update([0x1f]);
        hasher.update(part.as_bytes());
    }
    hasher.update([0x1f]);
    hasher.update(crate::review::fold_whitespace(content).as_bytes());
    hex(hasher)
}

/// Finalize a hasher as lowercase hex.
fn hex(hasher: Sha256) -> String {
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

// ---------------------------------------------------------------------------
// Slate capture (DR: plan D2/D3)
// ---------------------------------------------------------------------------

/// The feature vector recorded per slate member. `signals` holds the
/// retained rerank contributions — the exact values `--why` renders — as
/// `ContributionOutput`, the same serialized type. The six sibling groups
/// (TASK-105, DR-043, PRD-FB-REQ-021..024) are sparse, sorted maps
/// feature-name → value-label: string labels and bucket names, never raw
/// floats (REQ-023). Absent data means an absent key — never a defaulted
/// one (REQ-027) — and old rows without the siblings stay deserializable.
///
/// Flattening contract for TASK-102's learner: the learnable key is the
/// flattened string — `name=value` inside its group for scalar
/// categoricals (`kind=function`, `history:churn=high`) and the bare
/// presence key for ancestors and boolean context features
/// (`path:src/auth`, `context:same_file=yes`). Every value in these maps
/// is chosen from a fixed label set or the capped categorical space, so
/// the flattened key space is bounded by construction.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct FeatureGroups {
    /// One entry per active builtin signal, in registry order.
    #[serde(default)]
    pub signals: Vec<crate::output::ContributionOutput>,
    /// Ancestor-directory presence features (`src`, `src/auth`, ...) plus
    /// `class`/`depth`/`lang` (PRD-FB-REQ-022).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub path: BTreeMap<String, String>,
    /// Owning-symbol attributes; omitted for line-anchored members.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub symbol: BTreeMap<String, String>,
    /// Match shape: category, term coverage, anchoring.
    #[serde(rename = "match", default, skip_serializing_if = "BTreeMap::is_empty")]
    pub match_: BTreeMap<String, String>,
    /// Graph position at the result position; omitted without a topology
    /// row.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub graph: BTreeMap<String, String>,
    /// Modification history; the whole group is omitted without mined
    /// history.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub history: BTreeMap<String, String>,
    /// Author-derived features (PRD-FB-REQ-028); never built when
    /// `[feedback] author_features` is off.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub author: BTreeMap<String, String>,
    /// Context-relative features (PRD-FB-REQ-027); absent entirely without
    /// a working-context hint.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub context: BTreeMap<String, String>,
}

/// One result exactly as the caller saw it: identity, 1-based rank in the
/// flattened display order, and its full feature vector. `symbol`/`kind`
/// are `None` for file-level matches anchored by [`line_identity`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SlateMember {
    /// Content-anchored identity ([`result_identity`] or
    /// [`line_identity`]).
    pub identity: String,
    /// 1-based position in the flattened display order.
    pub rank: usize,
    /// Set by `record_feedback` on the reported-useful results; always
    /// `false` in a stored slate.
    pub chosen: bool,
    pub file: String,
    pub line: u64,
    pub symbol: Option<String>,
    pub kind: Option<String>,
    /// The pipeline score the caller saw.
    pub score: f32,
    pub groups: FeatureGroups,
}

/// One `symbols` row, bulk-loaded for span resolution. `file` is the
/// DB-stored repo-relative path — identities anchor on it, never on the
/// result path the caller saw (which may be absolute). `id`/`scope`/
/// `language` feed the descriptive features (TASK-105); the SELECT gained
/// them without adding a statement.
struct SymbolRow {
    id: i64,
    file: String,
    line: i64,
    end_line: Option<i64>,
    name: String,
    kind: String,
    scope: Option<String>,
    signature: Option<String>,
    language: String,
}

/// The owning symbol of `line` in `file`: the candidate with the SMALLEST
/// span containing it (`line <= target <= end_line`, NULL `end_line`
/// treated as `line`), tie-broken on (line desc, name) for determinism.
/// Returns `None` when no span contains the line.
fn owning_symbol(rows: &[SymbolRow], line: u64) -> Option<&SymbolRow> {
    let target = line as i64;
    rows.iter()
        .filter(|r| {
            let end = r.end_line.unwrap_or(r.line);
            r.line <= target && target <= end
        })
        .min_by(|a, b| {
            let span_a = a.end_line.unwrap_or(a.line) - a.line;
            let span_b = b.end_line.unwrap_or(b.line) - b.line;
            span_a
                .cmp(&span_b)
                .then_with(|| b.line.cmp(&a.line))
                .then_with(|| a.name.cmp(&b.name))
        })
}

/// Bulk-load the symbol rows of `files` (one bounded query per chunk).
///
/// Result paths are matched exactly first; a file with no exact rows is
/// re-queried by suffix — search paths may be absolute or `./`-prefixed
/// (the MCP surface passes absolute paths) while `symbols.file` is always
/// repo-relative, and the identity's stability depends on anchoring on
/// the DB path. The returned map is keyed by the REQUESTED file string so
/// callers resolve by what they hold.
fn load_symbols_by_file(
    conn: &Connection,
    files: &[String],
) -> Result<HashMap<String, Vec<SymbolRow>>> {
    let mut map: HashMap<String, Vec<SymbolRow>> = HashMap::new();
    for chunk in files.chunks(SQL_VAR_LIMIT) {
        let placeholders = vec!["?"; chunk.len()].join(", ");
        let sql = format!(
            "SELECT id, file, line, end_line, name, kind, scope, signature, language \
             FROM symbols WHERE file IN ({placeholders})"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(chunk.iter()), symbol_row)?
            .collect::<rusqlite::Result<Vec<SymbolRow>>>()?;
        for symbol in rows {
            map.entry(symbol.file.clone()).or_default().push(symbol);
        }
    }
    for file in files {
        if map.contains_key(file) {
            continue;
        }
        // Suffix resolution: `symbols.file` is a path-separator-boundary
        // suffix of the requested (possibly absolute) path.
        let sql = "SELECT id, file, line, end_line, name, kind, scope, signature, language \
                   FROM symbols WHERE ?1 LIKE '%' || file";
        let mut stmt = conn.prepare(sql)?;
        let rows = stmt
            .query_map([file], symbol_row)?
            .collect::<rusqlite::Result<Vec<SymbolRow>>>()?;
        let matched: Vec<SymbolRow> = rows
            .into_iter()
            .filter(|r| crate::rerank::is_path_suffix(file, &r.file))
            .collect();
        if !matched.is_empty() {
            map.insert(file.clone(), matched);
        }
    }
    Ok(map)
}

/// Map one query row to a [`SymbolRow`].
fn symbol_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<SymbolRow> {
    Ok(SymbolRow {
        id: row.get(0)?,
        file: row.get(1)?,
        line: row.get(2)?,
        end_line: row.get(3)?,
        name: row.get(4)?,
        kind: row.get(5)?,
        scope: row.get(6)?,
        signature: row.get(7)?,
        language: row.get(8)?,
    })
}

/// SQLite's default host-parameter limit; chunking keeps the IN-list
/// bounded no matter how many files a result set names.
const SQL_VAR_LIMIT: usize = 500;

// ---------------------------------------------------------------------------
// Descriptive feature extraction (TASK-105, DR-043)
// ---------------------------------------------------------------------------

// Bucket edges — fixed constants, not configuration (the CO_CHANGE_TOP_K
// precedent): bounds, not behavior, retunable by TASK-102/103 when
// feedback evidence exists. Rationale per scale:
// - body size (lines): 8 ≈ a getter, 30 ≈ a helper, 100 ≈ a function
//   with real logic, 400 ≈ a god-function.
// - fan degree: 5 = regular collaborators, 20 = hub territory (the same
//   order as reach's per-source fan-out cap).
// - recency: geometric-ish time bands (a day, a week, a month, a year).
// - churn (age-weighted commit count): 0 = untouched, 3 = occasional,
//   10 = hotbed; co-change mirrors the churn scale.
// - depth: 2 ancestor dirs keeps shallow utility code out of deep trees.
const BODY_SIZE_TINY_BELOW: i64 = 8;
const BODY_SIZE_SMALL_BELOW: i64 = 30;
const BODY_SIZE_MEDIUM_BELOW: i64 = 100;
const BODY_SIZE_LARGE_BELOW: i64 = 400;
const FAN_LOW_BELOW: u32 = 5;
const FAN_MEDIUM_BELOW: u32 = 20;
const DEPTH_MID_FROM: usize = 3;
const DEPTH_DEEP_FROM: usize = 5;
const RECENCY_HOURS_BELOW: i64 = 24 * 3600;
const RECENCY_DAYS_BELOW: i64 = 7 * 24 * 3600;
const RECENCY_WEEKS_BELOW: i64 = 30 * 24 * 3600;
const RECENCY_MONTHS_BELOW: i64 = 365 * 24 * 3600;
const CHURN_LOW_BELOW: f32 = 3.0;
const CHURN_MEDIUM_BELOW: f32 = 10.0;
const IMPORT_DISTANCE_TRANSITIVE_MAX: i64 = 3;

/// Per-slate cap on distinct values of any one categorical feature
/// (PRD-FB-REQ-024): beyond it the remaining values collapse into the
/// shared `__overflow__` label. 32 exceeds any plausible slate's
/// diversity, so it fires only on pathological sets.
const CATEGORICAL_CAP: usize = 32;
/// The shared overflow label every beyond-cap categorical value wears.
const OVERFLOW_LABEL: &str = "__overflow__";

/// Body-size bucket (lines). `tiny` < 8 < `small` < 30 < `medium` < 100 <
/// `large` < 400 <= `huge`.
fn body_size_bucket(lines: i64) -> &'static str {
    if lines < BODY_SIZE_TINY_BELOW {
        "tiny"
    } else if lines < BODY_SIZE_SMALL_BELOW {
        "small"
    } else if lines < BODY_SIZE_MEDIUM_BELOW {
        "medium"
    } else if lines < BODY_SIZE_LARGE_BELOW {
        "large"
    } else {
        "huge"
    }
}

/// Fan-degree bucket (fan-in or fan-out): `zero` (0) / `low` (<5) /
/// `medium` (<20) / `high` (>=20).
fn fan_bucket(degree: u32) -> &'static str {
    if degree == 0 {
        "zero"
    } else if degree < FAN_LOW_BELOW {
        "low"
    } else if degree < FAN_MEDIUM_BELOW {
        "medium"
    } else {
        "high"
    }
}

/// Tree-depth bucket over the ancestor-directory count: `shallow` (<=2) /
/// `mid` (3-4) / `deep` (>=5).
fn depth_bucket(ancestors: usize) -> &'static str {
    if ancestors < DEPTH_MID_FROM {
        "shallow"
    } else if ancestors < DEPTH_DEEP_FROM {
        "mid"
    } else {
        "deep"
    }
}

/// Recency bucket over the age in seconds: `hours` (<24h) / `days` (<7d)
/// / `weeks` (<30d) / `months` (<365d) / `ancient` (>=365d).
fn recency_bucket(age_secs: i64) -> &'static str {
    if age_secs < RECENCY_HOURS_BELOW {
        "hours"
    } else if age_secs < RECENCY_DAYS_BELOW {
        "days"
    } else if age_secs < RECENCY_WEEKS_BELOW {
        "weeks"
    } else if age_secs < RECENCY_MONTHS_BELOW {
        "months"
    } else {
        "ancient"
    }
}

/// Churn bucket over the age-weighted commit count: `zero` (=0) / `low`
/// (<3) / `medium` (<10) / `high` (>=10).
fn churn_bucket(score: f32) -> &'static str {
    if score <= 0.0 {
        "zero"
    } else if score < CHURN_LOW_BELOW {
        "low"
    } else if score < CHURN_MEDIUM_BELOW {
        "medium"
    } else {
        "high"
    }
}

/// Co-change bucket over the hint-file coupling weight: `none` (=0) /
/// `weak` (<3) / `strong` (>=3) — the churn scale mirrored.
fn co_change_bucket(weight: f32) -> &'static str {
    if weight <= 0.0 {
        "none"
    } else if weight < CHURN_LOW_BELOW {
        "weak"
    } else {
        "strong"
    }
}

/// Term-coverage bucket over the covered fraction of query terms: `all`
/// (=1.0) / `most` (>=0.5) / `some` (>0) / `none` (0).
fn coverage_bucket(covered: usize, total: usize) -> &'static str {
    if total == 0 || covered == 0 {
        "none"
    } else if covered == total {
        "all"
    } else if (covered as f64) / (total as f64) >= 0.5 {
        "most"
    } else {
        "some"
    }
}

/// The lowercase label of a path class (the `PathClass` debug name).
fn path_class_label(class: crate::rerank::PathClass) -> &'static str {
    match class {
        crate::rerank::PathClass::Ordinary => "ordinary",
        crate::rerank::PathClass::Test => "test",
        crate::rerank::PathClass::Barrel => "barrel",
        crate::rerank::PathClass::ModuleEntry => "module_entry",
        crate::rerank::PathClass::Example => "example",
        crate::rerank::PathClass::Shim => "shim",
        crate::rerank::PathClass::TypeDeclaration => "type_declaration",
        crate::rerank::PathClass::Generated => "generated",
        crate::rerank::PathClass::GeneratedShadowed => "generated_shadowed",
    }
}

/// The result-category label the `match:category` feature records.
fn category_label(category: crate::ranker::ResultCategory) -> String {
    category.to_string()
}

/// The proper ancestor-directory prefixes of a canonical repo-relative
/// path, outermost first: `src/auth/tokens/issue.rs` ->
/// `[src, src/auth, src/auth/tokens]`.
fn ancestor_dirs(canonical: &str) -> Vec<String> {
    let segments: Vec<&str> = canonical.split('/').collect();
    let mut ancestors = Vec::with_capacity(segments.len().saturating_sub(1));
    let mut current = String::new();
    for segment in &segments[..segments.len().saturating_sub(1)] {
        if !current.is_empty() {
            current.push('/');
        }
        current.push_str(segment);
        ancestors.push(current.clone());
    }
    ancestors
}

/// Everything the pure extractor may read (D7): the result, its owning
/// symbol row, the shared context the pipeline already prepared, the
/// query, one clock reading, and the author switch. Nothing else — no
/// connection, so extraction is a pure function with no round trips.
struct ExtractionInputs<'a> {
    query: &'a str,
    ctx: &'a crate::rerank::SharedContext,
    /// Wall clock captured once per slate (the `created_at` precedent):
    /// same-input determinism holds except across bucket boundaries.
    now: SystemTime,
    author_features: bool,
}

/// Extract one member's descriptive groups (the D2 catalogue, one clause
/// per row). Pure over its inputs; members with no owning symbol
/// (line-anchored) get `path`/`match`/`history`/`author`/`context` only —
/// `symbol`/`graph` keys are omitted for them, never defaulted.
fn extract_groups(
    item: &crate::rerank::ScoredResult,
    owning: Option<&SymbolRow>,
    canonical: &str,
    inputs: &ExtractionInputs<'_>,
) -> FeatureGroups {
    let result = &item.classified.result;
    let ctx = inputs.ctx;
    let mut groups = FeatureGroups::default();

    // -- path (PRD-FB-REQ-021/022) -------------------------------------------
    for ancestor in ancestor_dirs(canonical) {
        groups.path.insert(ancestor, "1".to_string());
    }
    let class = ctx
        .path_class(canonical)
        .unwrap_or_else(|| crate::rerank::classify_path_character(std::path::Path::new(canonical)));
    groups
        .path
        .insert("class".to_string(), path_class_label(class).to_string());
    groups.path.insert(
        "depth".to_string(),
        depth_bucket(ancestor_dirs(canonical).len()).to_string(),
    );
    if let Some(sym) = owning {
        groups.path.insert("lang".to_string(), sym.language.clone());
    }

    // -- symbol ---------------------------------------------------------------
    if let Some(sym) = owning {
        groups.symbol.insert("kind".to_string(), sym.kind.clone());
        let scoped = match sym.scope.as_deref() {
            None | Some("") => "top_level",
            Some(_) => "nested",
        };
        groups
            .symbol
            .insert("scoped".to_string(), scoped.to_string());
        let name_match = if sym.name.eq_ignore_ascii_case(inputs.query) {
            "exact"
        } else if sym
            .name
            .to_ascii_lowercase()
            .contains(&inputs.query.to_ascii_lowercase())
        {
            "substring"
        } else {
            "other"
        };
        groups
            .symbol
            .insert("name_match".to_string(), name_match.to_string());
        let body = sym.end_line.unwrap_or(sym.line) - sym.line + 1;
        groups
            .symbol
            .insert("body_size".to_string(), body_size_bucket(body).to_string());
    }

    // -- match ----------------------------------------------------------------
    groups.match_.insert(
        "category".to_string(),
        category_label(item.classified.category),
    );
    let terms: Vec<String> = if ctx.terms().is_empty() {
        crate::tokenizer::tokenize(inputs.query)
    } else {
        ctx.terms().to_vec()
    };
    let covered = terms
        .iter()
        .filter(|term| {
            let in_line = crate::rerank::contains_identifier_token(&result.content, term);
            let in_symbol = owning.is_some_and(|sym| {
                let name_and_signature =
                    format!("{} {}", sym.name, sym.signature.as_deref().unwrap_or(""));
                crate::rerank::contains_identifier_token(&name_and_signature, term)
            });
            in_line || in_symbol
        })
        .count();
    groups.match_.insert(
        "term_coverage".to_string(),
        coverage_bucket(covered, terms.len()).to_string(),
    );
    groups.match_.insert(
        "anchored".to_string(),
        if owning.is_some() { "symbol" } else { "line" }.to_string(),
    );

    // -- graph ----------------------------------------------------------------
    // Position-keyed through the dual-keyed context: the canonical file is
    // always present as a key.
    if let (Some(fan_in), Some(fan_out)) = (
        ctx.fan_in_at(canonical, result.line),
        ctx.fan_out_at(canonical, result.line),
    ) {
        groups
            .graph
            .insert("fan_in".to_string(), fan_bucket(fan_in).to_string());
        groups
            .graph
            .insert("fan_out".to_string(), fan_bucket(fan_out).to_string());
    }

    // -- history (whole group omitted without mined history) ------------------
    if !ctx.churn_scored_files() {
        // No history tables / never mined: the group is absent, not
        // defaulted.
    } else {
        let age = inputs
            .now
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0)
            .saturating_sub(ctx.last_ts_of(canonical).unwrap_or(0));
        groups
            .history
            .insert("recency".to_string(), recency_bucket(age).to_string());
        groups.history.insert(
            "churn".to_string(),
            churn_bucket(ctx.churn_score(canonical).unwrap_or(0.0)).to_string(),
        );
        if inputs.author_features {
            if let Some(author) = ctx.last_author_of(canonical) {
                groups
                    .author
                    .insert("last_touched_by".to_string(), author.to_string());
            }
            if let Some(author) = ctx.primary_author_of(canonical) {
                groups
                    .author
                    .insert("primary".to_string(), author.to_string());
            }
        }
    }

    // -- context (PRD-FB-REQ-027: absent rather than defaulted) ----------------
    if let Some(raw_hint) = ctx.working_hint() {
        let hint = raw_hint.strip_prefix("./").unwrap_or(raw_hint);
        let resolved = ctx.working_hint_path();
        let hint_file = resolved.cloned().unwrap_or_else(|| hint.to_string());
        let same_file = resolved
            .map(|r| r == canonical)
            .unwrap_or_else(|| crate::rerank::is_path_suffix(hint, canonical));
        groups.context.insert(
            "same_file".to_string(),
            if same_file { "yes" } else { "no" }.to_string(),
        );
        let same_dir = parent_dir(&hint_file) == parent_dir(canonical);
        groups.context.insert(
            "same_directory".to_string(),
            if same_dir { "yes" } else { "no" }.to_string(),
        );
        match (
            ctx.working_hint_community(),
            ctx.community_at(canonical, result.line),
        ) {
            (Some(hint_community), Some(member_community)) => {
                groups.context.insert(
                    "same_community".to_string(),
                    if hint_community == member_community {
                        "yes"
                    } else {
                        "no"
                    }
                    .to_string(),
                );
            }
            _ => {
                // Either side's community is unknown: omit, never default.
            }
        }
        if let Some(sym) = owning {
            let distance = ctx
                .import_distance(sym.id)
                .map(|d| {
                    if d == 1 {
                        "direct"
                    } else if d <= IMPORT_DISTANCE_TRANSITIVE_MAX {
                        "transitive"
                    } else {
                        "unreachable"
                    }
                })
                .unwrap_or("unreachable");
            groups
                .context
                .insert("import_distance".to_string(), distance.to_string());
        }
        groups.context.insert(
            "co_change".to_string(),
            co_change_bucket(ctx.co_change_partner(canonical).unwrap_or(0.0)).to_string(),
        );
    }
    groups
}

/// The parent directory of a `/`-separated path (`""` for a root file).
fn parent_dir(path: &str) -> &str {
    match path.rfind('/') {
        Some(at) => &path[..at],
        None => "",
    }
}

/// The scalar (non-ancestor) feature names of the `path` group — every
/// other `path` key is an ancestor presence feature.
const PATH_SCALARS: [&str; 3] = ["class", "depth", "lang"];

/// Cap per-slate categorical cardinality (PRD-FB-REQ-024, D8): for every
/// scalar feature, and for the `path` ancestor keys as one family, keep
/// the [`CATEGORICAL_CAP`] most frequent values (count desc, then value
/// asc — a total order, so HashMap order cannot leak) and collapse every
/// other occurrence into the shared `__overflow__` label (ancestors
/// collapse to one `__overflow__` presence key). Fixed-label bucket
/// features are closed sets; the rule passes over them harmlessly.
fn apply_cardinality_cap(groups: &mut [FeatureGroups]) {
    use std::collections::{BTreeSet, HashMap as CountMap};

    // (group name, feature name) -> value -> count, across all members.
    let mut counts: CountMap<(&'static str, String), CountMap<String, usize>> = CountMap::new();
    for member in groups.iter() {
        for (group_name, map) in [
            ("symbol", &member.symbol),
            ("match", &member.match_),
            ("graph", &member.graph),
            ("history", &member.history),
            ("author", &member.author),
            ("context", &member.context),
        ] {
            for (name, value) in map {
                *counts
                    .entry((group_name, name.clone()))
                    .or_default()
                    .entry(value.clone())
                    .or_insert(0) += 1;
            }
        }
        for (name, value) in &member.path {
            if PATH_SCALARS.contains(&name.as_str()) {
                *counts
                    .entry(("path", name.clone()))
                    .or_default()
                    .entry(value.clone())
                    .or_insert(0) += 1;
            }
        }
    }

    // The kept set per feature: most frequent first, ties to the smaller
    // value.
    let mut kept: CountMap<(&'static str, String), BTreeSet<String>> = CountMap::new();
    for (key, values) in &counts {
        if values.len() <= CATEGORICAL_CAP {
            continue;
        }
        let mut ranked: Vec<(&String, &usize)> = values.iter().collect();
        ranked.sort_by(|(value_a, count_a), (value_b, count_b)| {
            count_b.cmp(count_a).then_with(|| value_a.cmp(value_b))
        });
        kept.insert(
            key.clone(),
            ranked
                .into_iter()
                .take(CATEGORICAL_CAP)
                .map(|(value, _)| value.clone())
                .collect(),
        );
    }

    // The ancestor family, as one pool of distinct keys.
    let mut ancestors: CountMap<String, usize> = CountMap::new();
    for member in groups.iter() {
        for name in member.path.keys() {
            if !PATH_SCALARS.contains(&name.as_str()) {
                *ancestors.entry(name.clone()).or_insert(0) += 1;
            }
        }
    }
    let kept_ancestors = if ancestors.len() > CATEGORICAL_CAP {
        let mut ranked: Vec<(&String, &usize)> = ancestors.iter().collect();
        ranked.sort_by(|(key_a, count_a), (key_b, count_b)| {
            count_b.cmp(count_a).then_with(|| key_a.cmp(key_b))
        });
        Some(
            ranked
                .into_iter()
                .take(CATEGORICAL_CAP)
                .map(|(key, _)| key.clone())
                .collect::<BTreeSet<_>>(),
        )
    } else {
        None
    };

    for member in groups.iter_mut() {
        for (group_name, map) in [
            ("symbol", &mut member.symbol),
            ("match", &mut member.match_),
            ("graph", &mut member.graph),
            ("history", &mut member.history),
            ("author", &mut member.author),
            ("context", &mut member.context),
        ] {
            for (name, value) in map.iter_mut() {
                if let Some(keep) = kept.get(&(group_name, name.clone()))
                    && !keep.contains(value.as_str())
                {
                    *value = OVERFLOW_LABEL.to_string();
                }
            }
        }
        for (name, value) in member.path.iter_mut() {
            if PATH_SCALARS.contains(&name.as_str())
                && let Some(keep) = kept.get(&("path", name.clone()))
                && !keep.contains(value.as_str())
            {
                *value = OVERFLOW_LABEL.to_string();
            }
        }
        if let Some(keep) = &kept_ancestors {
            let overflowed: Vec<String> = member
                .path
                .keys()
                .filter(|name| !PATH_SCALARS.contains(&name.as_str()) && !keep.contains(*name))
                .cloned()
                .collect();
            let had_overflow = !overflowed.is_empty();
            for key in overflowed {
                member.path.remove(&key);
            }
            if had_overflow {
                member
                    .path
                    .insert(OVERFLOW_LABEL.to_string(), "1".to_string());
            }
        }
    }
}

/// Build the slate members for a ranked search: flattened display order,
/// each member identity-anchored on its owning symbol's DB row (or the
/// matched line when no symbol owns it), carrying the retained signal
/// contributions verbatim plus the descriptive feature groups extracted
/// PURELY over the shared context the pipeline already prepared
/// (TASK-105). The symbol bulk-load is the ONLY statement set the slate
/// build issues — features add no round trips beyond the batched prepare
/// (the trace test enforces it).
fn build_members(
    conn: &Connection,
    query: &str,
    ranked: &crate::rerank::RankedSearch,
    feedback: &crate::config::FeedbackConfig,
) -> Result<Vec<SlateMember>> {
    let flat: Vec<&crate::rerank::ScoredResult> =
        ranked.groups.iter().flat_map(|(_, g)| g.iter()).collect();
    let files: Vec<String> = {
        let mut seen = std::collections::BTreeSet::new();
        for item in &flat {
            seen.insert(item.classified.result.file.to_string_lossy().into_owned());
        }
        seen.into_iter().collect()
    };
    let symbols = load_symbols_by_file(conn, &files)?;
    let inputs = ExtractionInputs {
        query,
        ctx: &ranked.context,
        now: SystemTime::now(),
        author_features: feedback.author_features,
    };
    let mut extracted: Vec<FeatureGroups> = Vec::with_capacity(flat.len());
    let mut members = Vec::with_capacity(flat.len());
    for (idx, item) in flat.iter().enumerate() {
        let result = &item.classified.result;
        let file = result.file.to_string_lossy().into_owned();
        let rows = symbols.get(&file).map(Vec::as_slice).unwrap_or(&[]);
        let (identity, symbol, kind, canonical) = match owning_symbol(rows, result.line) {
            Some(sym) => (
                // Anchor on the DB-stored repo-relative path: re-indexing
                // re-inserts the same row, wherever the repo is checked out.
                result_identity(
                    &sym.file,
                    &sym.kind,
                    &sym.name,
                    sym.signature.as_deref().unwrap_or(""),
                ),
                Some(sym.name.clone()),
                Some(sym.kind.clone()),
                sym.file.clone(),
            ),
            None => {
                // Line-anchored: the canonical key the prepare resolved,
                // falling back to the raw string stripped of a leading
                // `./` — feature names must be identical for the same
                // result whether it arrived over CLI or MCP.
                let canonical = ranked
                    .context
                    .canonical_file(&file)
                    .map(str::to_string)
                    .unwrap_or_else(|| file.strip_prefix("./").unwrap_or(&file).to_string());
                (
                    line_identity(
                        &canonical,
                        &item.classified.category.to_string(),
                        &result.content,
                    ),
                    None,
                    None,
                    canonical,
                )
            }
        };
        extracted.push(extract_groups(
            item,
            owning_symbol(rows, result.line),
            &canonical,
            &inputs,
        ));
        members.push(SlateMember {
            identity,
            rank: idx + 1,
            chosen: false,
            file,
            line: result.line,
            symbol,
            kind,
            score: item.score,
            groups: FeatureGroups::default(),
        });
    }
    apply_cardinality_cap(&mut extracted);
    for (member, mut groups) in members.iter_mut().zip(extracted) {
        groups.signals = crate::output::WhyOutput::from_contributions(
            member.score,
            &flat[member.rank - 1].contributions,
        )
        .signals;
        member.groups = groups;
    }
    Ok(members)
}

/// 16-hex-char slate token: first 8 bytes of SHA-256 over deterministic
/// inputs plus the mint-time nanos and a collision-retry nonce.
fn slate_token(query: &str, nanos: u128, members: &[SlateMember], nonce: u32) -> String {
    let mut hasher = Sha256::new();
    hasher.update(query.as_bytes());
    for part in [
        nanos.to_string(),
        members.len().to_string(),
        members
            .first()
            .map(|m| m.identity.as_str())
            .unwrap_or("")
            .to_string(),
        nonce.to_string(),
    ] {
        hasher.update([0x1f]);
        hasher.update(part.as_bytes());
    }
    hex(hasher)[..16].to_string()
}

/// Prune `feedback_slates` to the newest `retention` rows (LRU by
/// `created_at`, token breaking ties deterministically).
pub fn prune_slates(conn: &Connection, retention: usize) -> Result<()> {
    conn.execute(
        "DELETE FROM feedback_slates WHERE token NOT IN \
         (SELECT token FROM feedback_slates ORDER BY created_at DESC, token DESC LIMIT ?1)",
        [retention as i64],
    )?;
    Ok(())
}

/// A persisted slate: the token echoed to the caller plus the members,
/// so the dispatch layer can stamp per-row `identity` fields without
/// re-deriving identities.
#[derive(Debug, Clone)]
pub struct StoredSlate {
    pub token: String,
    pub members: Vec<SlateMember>,
}

/// Build the slate for `ranked` and persist it as one `feedback_slates`
/// row, pruned to `feedback.slate_retention`, in one transaction. Returns
/// the echoed token with the members. The caller gates this on
/// `[feedback] enabled` AND the pipeline having run — a legacy-path slate
/// carries no contributions to learn from. `feedback.author_features`
/// switches the author-derived group (PRD-FB-REQ-028).
pub fn build_and_store_slate(
    conn: &Connection,
    query: &str,
    ranked: &crate::rerank::RankedSearch,
    feedback: &crate::config::FeedbackConfig,
) -> Result<StoredSlate> {
    let members = build_members(conn, query, ranked, feedback)?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?;
    let query_class = ranked.query_class.map(|c| c.as_str().to_string());
    let members_json = serde_json::to_string(&members)?;
    let tx = conn.unchecked_transaction()?;
    let mut token = String::new();
    for nonce in 0..3 {
        let candidate = slate_token(query, now.as_nanos(), &members, nonce);
        let inserted = tx.execute(
            "INSERT INTO feedback_slates (token, query, query_class, members, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![
                candidate,
                query,
                query_class,
                members_json,
                now.as_secs() as i64
            ],
        );
        match inserted {
            Ok(_) => {
                token = candidate;
                break;
            }
            Err(rusqlite::Error::SqliteFailure(e, _))
                if e.code == rusqlite::ErrorCode::ConstraintViolation =>
            {
                continue;
            }
            Err(e) => return Err(e.into()),
        }
    }
    if token.is_empty() {
        bail!("could not mint a unique slate token after 3 attempts");
    }
    prune_slates(&tx, feedback.slate_retention)?;
    tx.commit()?;
    Ok(StoredSlate { token, members })
}

// ---------------------------------------------------------------------------
// Feedback recording + read APIs
// ---------------------------------------------------------------------------

/// The `feedback_events.features` payload: the full slate as persisted at
/// search time, with `chosen` set on the reported-useful members. Every
/// event of one feedback call carries the same document.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SlateFeatures {
    /// Shape version; bump on breaking change.
    pub schema: u32,
    /// The slate token this event was reported against.
    pub slate: String,
    /// Every result that was shown, alternatives included (PRD-FB-REQ-002).
    pub members: Vec<SlateMember>,
}

/// One recorded event as the caller-facing summary reports it.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct RecordedEvent {
    pub identity: String,
    pub rank: usize,
    pub file: String,
    pub line: u64,
    pub symbol: Option<String>,
    /// Whether the identity still resolves against the current index.
    /// Line-anchored members (`symbol: None`) count as live: their
    /// identity is content-derived and never retires by drift.
    pub live: bool,
}

/// The result of one feedback call — the summary both the CLI and the
/// `wonk_feedback` MCP tool render.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct FeedbackSummary {
    /// Events written (one per reported-useful result).
    pub recorded: usize,
    pub query: String,
    pub query_class: Option<String>,
    pub events: Vec<RecordedEvent>,
}

/// One `feedback_events` row, typed, so TASK-102 never parses the JSON
/// itself.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FeedbackEvent {
    pub id: i64,
    pub result_identity: String,
    pub query_class: Option<String>,
    pub chosen_rank: i64,
    pub features: SlateFeatures,
    pub useful: bool,
    pub session: Option<String>,
    pub created_at: i64,
}

/// Load every feedback event, oldest first, features parsed.
pub fn load_events(conn: &Connection) -> Result<Vec<FeedbackEvent>> {
    let mut stmt = conn.prepare(
        "SELECT id, result_identity, query_class, chosen_rank, features, useful, session, \
         created_at FROM feedback_events ORDER BY id",
    )?;
    let events = stmt
        .query_map([], |row| {
            Ok(FeedbackEvent {
                id: row.get(0)?,
                result_identity: row.get(1)?,
                query_class: row.get(2)?,
                chosen_rank: row.get(3)?,
                features: serde_json::from_str(&row.get::<_, String>(4)?).map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(
                        4,
                        rusqlite::types::Type::Text,
                        Box::new(e),
                    )
                })?,
                useful: row.get::<_, i64>(5)? == 1,
                session: row.get(6)?,
                created_at: row.get(7)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(events)
}

/// Distinct sessions that have reported `identity` useful — the exact
/// observation count TASK-102's gate and TASK-104's multi-session gate
/// consume (PRD-FB-REQ-015/016).
pub fn distinct_sessions(conn: &Connection, identity: &str) -> i64 {
    conn.query_row(
        "SELECT COUNT(DISTINCT session) FROM feedback_events WHERE result_identity = ?1",
        [identity],
        |row| row.get(0),
    )
    .unwrap_or(0)
}

/// Maximum session id length (D5).
const SESSION_MAX: usize = 256;

/// Validate a session id: non-empty after trim, at most [`SESSION_MAX`]
/// chars.
fn validate_session(session: &str) -> Result<()> {
    if session.trim().is_empty() {
        bail!("session must be a non-empty id identifying your current session/conversation");
    }
    if session.chars().count() > SESSION_MAX {
        bail!("session must be at most {SESSION_MAX} characters");
    }
    Ok(())
}

/// Record feedback against a persisted slate (PRD-FB-REQ-001/002/003).
///
/// `useful` names results by identity (64-hex) or 1-based rank; one
/// `feedback_events` row is written per useful member, each carrying the
/// FULL slate in `features` with `chosen` set on exactly the useful
/// members. All resolution happens before the first write, so a bad
/// argument leaves the table untouched. Returns the summary both surfaces
/// render, with read-time liveness per event.
pub fn record_feedback(
    conn: &Connection,
    token: &str,
    useful: &[String],
    session: &str,
) -> Result<FeedbackSummary> {
    validate_session(session)?;
    let trimmed: Vec<&str> = useful
        .iter()
        .map(|u| u.trim())
        .filter(|u| !u.is_empty())
        .collect();
    if trimmed.is_empty() {
        bail!("useful must name at least one result (identity or 1-based rank)");
    }
    let (query, query_class, mut members): (String, Option<String>, Vec<SlateMember>) = conn
        .query_row(
            "SELECT query, query_class, members FROM feedback_slates WHERE token = ?1",
            [token],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    serde_json::from_str::<Vec<SlateMember>>(&row.get::<_, String>(2)?).map_err(
                        |e| {
                            rusqlite::Error::FromSqlConversionFailure(
                                2,
                                rusqlite::types::Type::Text,
                                Box::new(e),
                            )
                        },
                    )?,
                ))
            },
        )
        .map_err(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => anyhow::anyhow!(
                "slate not found (expired or pruned); re-run the search and report \
                 against the new slate"
            ),
            other => other.into(),
        })?;

    // Resolve every useful reference to a member index BEFORE writing.
    let mut chosen: Vec<usize> = Vec::new();
    for spec in &trimmed {
        let idx = match spec.parse::<usize>() {
            Ok(rank) => members
                .iter()
                .position(|m| m.rank == rank)
                .ok_or_else(|| anyhow::anyhow!("rank {rank} is not in the slate"))?,
            Err(_) => members
                .iter()
                .position(|m| m.identity == *spec)
                .ok_or_else(|| anyhow::anyhow!("identity '{spec}' is not in the slate"))?,
        };
        if !chosen.contains(&idx) {
            chosen.push(idx);
        }
    }
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64;

    // Read-time liveness (D7): recompute the identities of the files the
    // chosen members live in.
    let files: Vec<String> = {
        let mut seen = std::collections::BTreeSet::new();
        for &idx in &chosen {
            seen.insert(members[idx].file.clone());
        }
        seen.into_iter().collect()
    };
    let identities: std::collections::HashSet<String> = chosen
        .iter()
        .map(|&idx| members[idx].identity.clone())
        .collect();
    let live = live_identities(conn, &files, &identities);

    for &idx in &chosen {
        members[idx].chosen = true;
    }
    let features = SlateFeatures {
        schema: 1,
        slate: token.to_string(),
        members,
    };
    let features_json = serde_json::to_string(&features)?;

    let tx = conn.unchecked_transaction()?;
    for &idx in &chosen {
        let member = &features.members[idx];
        tx.execute(
            "INSERT INTO feedback_events \
             (result_identity, query_class, chosen_rank, features, useful, session, created_at) \
             VALUES (?1, ?2, ?3, ?4, 1, ?5, ?6)",
            rusqlite::params![
                member.identity,
                query_class,
                member.rank as i64,
                features_json,
                session,
                now
            ],
        )?;
    }
    tx.commit()?;

    let events: Vec<RecordedEvent> = chosen
        .into_iter()
        .map(|idx| {
            let m = &features.members[idx];
            let is_live = m.symbol.is_none() || live.contains(&m.identity);
            RecordedEvent {
                identity: m.identity.clone(),
                rank: m.rank,
                file: m.file.clone(),
                line: m.line,
                symbol: m.symbol.clone(),
                live: is_live,
            }
        })
        .collect();
    Ok(FeedbackSummary {
        recorded: events.len(),
        query,
        query_class,
        events,
    })
}

/// Which of `identities` still resolve against the current index (D7)?
///
/// Recomputes [`result_identity`] over the `symbols` rows of `files`
/// (exact-then-suffix resolution, the same anchoring the slate builder
/// used) and intersects: a rename, kind change, signature-token change,
/// or file move yields a different identity and the entry no longer
/// applies (PRD-FB-REQ-006) — retirement resolved at read time, never a
/// write. Body-only edits and re-indexing re-insert the same
/// `(file, kind, name, signature)` row, so those identities survive
/// (PRD-FB-REQ-005). Line-anchored identities are content-derived and not
/// recomputable from the index; they never match and are treated as live
/// by their consumers (they carry `symbol: null`).
pub fn live_identities(
    conn: &Connection,
    files: &[String],
    identities: &std::collections::HashSet<String>,
) -> std::collections::HashSet<String> {
    let mut live = std::collections::HashSet::new();
    if files.is_empty() || identities.is_empty() {
        return live;
    }
    let Ok(symbols) = load_symbols_by_file(conn, files) else {
        return live;
    };
    for rows in symbols.values() {
        for sym in rows {
            let identity = result_identity(
                &sym.file,
                &sym.kind,
                &sym.name,
                sym.signature.as_deref().unwrap_or(""),
            );
            if identities.contains(&identity) {
                live.insert(identity);
            }
        }
    }
    live
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- result_identity -------------------------------------------------------

    #[test]
    fn result_identity_is_stable_across_calls() {
        let a = result_identity(
            "src/auth.rs",
            "function",
            "handle_login",
            "fn handle_login(u: &User) -> Result<Token>",
        );
        let b = result_identity(
            "src/auth.rs",
            "function",
            "handle_login",
            "fn handle_login(u: &User) -> Result<Token>",
        );
        assert_eq!(a, b);
        assert_eq!(a.len(), 64, "SHA-256 hex: {a}");
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn result_identity_folds_signature_whitespace() {
        let tight = result_identity("a.rs", "function", "f", "fn f(x: i32) -> i32 {");
        let reflowed = result_identity("a.rs", "function", "f", "fn   f(x: i32)\t->  i32 {");
        assert_eq!(tight, reflowed, "signature reflow must not change identity");
    }

    #[test]
    fn result_identity_differs_per_component() {
        let base = result_identity("a.rs", "function", "f", "fn f(x: i32)");
        assert_ne!(
            result_identity("b.rs", "function", "f", "fn f(x: i32)"),
            base,
            "file change must change identity"
        );
        assert_ne!(
            result_identity("a.rs", "method", "f", "fn f(x: i32)"),
            base,
            "kind change must change identity"
        );
        assert_ne!(
            result_identity("a.rs", "function", "g", "fn f(x: i32)"),
            base,
            "name change must change identity"
        );
        assert_ne!(
            result_identity("a.rs", "function", "f", "fn f(y: i32)"),
            base,
            "signature token change must change identity"
        );
    }

    #[test]
    fn result_identity_is_distinct_from_line_identity() {
        let result = result_identity("a.rs", "function", "f", "fn f(x: i32)");
        let line = line_identity("a.rs", "definition", "fn f(x: i32)");
        assert_ne!(result, line);
    }

    // -- line_identity ----------------------------------------------------------

    #[test]
    fn line_identity_differs_per_category_and_content() {
        let base = line_identity("a.rs", "definition", "let x = compute();");
        assert_ne!(
            line_identity("a.rs", "call", "let x = compute();"),
            base,
            "category change must change identity"
        );
        assert_ne!(
            line_identity("a.rs", "definition", "let y = compute();"),
            base,
            "content change must change identity"
        );
        assert_eq!(
            base,
            line_identity("a.rs", "definition", "let  x =  compute();")
        );
    }

    // -- slate build/store/prune -------------------------------------------------
    //
    // Fixtures run the real `build_index` over a tempdir repo so the
    // symbol-span resolution sees genuine `symbols` rows.

    use rusqlite::Connection;
    use tempfile::TempDir;

    /// `outer_guard` contains `helper_inner` contains the `doubled` lines —
    /// a nest deep enough to make smallest-span resolution observable.
    const NESTED_SRC: &str = "pub fn outer_guard(a: u32) -> u32 {\n    fn helper_inner(x: u32) -> u32 {\n        let doubled = x * 2;\n        doubled + 1\n    }\n    helper_inner(a)\n}\n";

    /// Top-level lines outside any function span (a comment and a const)
    /// exercise the `line_identity` fallback.
    const OUTSIDE_SRC: &str = "// file-level note about tuning\nconst TUNING_LIMIT: u32 = 7;\n\npub fn tuned(v: u32) -> u32 {\n    v.min(TUNING_LIMIT)\n}\n";

    /// An unrelated second file for cross-file retirement checks.
    const OTHER_SRC: &str = "pub fn other_entry(x: i64) -> i64 {\n    x.abs()\n}\n";

    fn seeded_conn(files: &[(&str, &str)]) -> (TempDir, Connection) {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        std::fs::create_dir(root.join(".git")).unwrap();
        for (name, src) in files {
            if let Some(parent) = std::path::Path::new(name).parent() {
                std::fs::create_dir_all(root.join(parent)).unwrap();
            }
            std::fs::write(root.join(name), src).unwrap();
        }
        crate::pipeline::build_index(root, true).unwrap();
        let index_path = crate::db::find_existing_index(root).unwrap();
        let conn = crate::db::open(&index_path).unwrap();
        (dir, conn)
    }

    /// One synthetic hit: (file, line, content, score).
    type Hit<'a> = (&'a str, u64, &'a str, f32);

    /// A synthetic ranked search over the given hits: pipeline-shaped
    /// `ScoredResult`s (contributions carried verbatim) in the given
    /// group order, exactly what the dispatch layer holds post-ranking.
    fn ranked_search(
        groups: Vec<(crate::ranker::ResultCategory, Vec<Hit<'_>>)>,
    ) -> crate::rerank::RankedSearch {
        let groups = groups
            .into_iter()
            .map(|(category, hits)| {
                let items = hits
                    .into_iter()
                    .map(|(file, line, content, score)| crate::rerank::ScoredResult {
                        classified: crate::ranker::ClassifiedResult {
                            result: crate::search::SearchResult {
                                file: std::path::PathBuf::from(file),
                                line,
                                col: 1,
                                content: content.to_string(),
                            },
                            category,
                            annotation: None,
                        },
                        score,
                        contributions: vec![crate::rerank::Contribution {
                            signal: "kind",
                            value: score,
                            weight: 1.0,
                            weighted: score,
                        }],
                    })
                    .collect();
                (category, items)
            })
            .collect();
        crate::rerank::RankedSearch {
            groups,
            query_class: Some(crate::rerank::QueryClass::Symbol),
            near_duplicates: Vec::new(),
            context: Default::default(),
        }
    }

    fn slate_row(conn: &Connection, token: &str) -> (String, Option<String>, String) {
        conn.query_row(
            "SELECT query, query_class, members FROM feedback_slates WHERE token = ?1",
            [token],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap()
    }

    #[test]
    fn owning_symbol_resolution_picks_smallest_containing_span() {
        let (dir, conn) = seeded_conn(&[("nested.rs", NESTED_SRC)]);
        let ranked = ranked_search(vec![(
            crate::ranker::ResultCategory::Definition,
            // Line 3 (`let doubled = ...`) sits inside BOTH outer_guard
            // (1..6) and helper_inner (2..5); the smaller span owns it.
            vec![("nested.rs", 3, "        let doubled = x * 2;", 0.9)],
        )]);
        let token = build_and_store_slate(&conn, "doubled", &ranked, &test_feedback())
            .unwrap()
            .token;
        let (_, _, members_json) = slate_row(&conn, &token);
        let members: Vec<SlateMember> = serde_json::from_str(&members_json).unwrap();
        assert_eq!(members.len(), 1);
        let m = &members[0];
        assert_eq!(m.symbol.as_deref(), Some("helper_inner"));
        assert_eq!(m.kind.as_deref(), Some("function"));
        assert_eq!(
            m.identity,
            result_identity(
                "nested.rs",
                "function",
                "helper_inner",
                "fn helper_inner(x: u32) -> u32"
            ),
            "identity anchored on the owning symbol's stored signature"
        );
        drop(dir);
    }

    #[test]
    fn unresolved_line_falls_back_to_line_identity() {
        let (dir, conn) = seeded_conn(&[("outside.rs", OUTSIDE_SRC)]);
        let ranked = ranked_search(vec![(
            crate::ranker::ResultCategory::Comment,
            // Line 1 (the comment) is outside every symbol span.
            vec![("outside.rs", 1, "// file-level note about tuning", 0.5)],
        )]);
        let token = build_and_store_slate(&conn, "tuning", &ranked, &test_feedback())
            .unwrap()
            .token;
        let (_, _, members_json) = slate_row(&conn, &token);
        let members: Vec<SlateMember> = serde_json::from_str(&members_json).unwrap();
        let m = &members[0];
        assert_eq!(m.symbol, None, "no owning symbol");
        assert_eq!(m.kind, None);
        assert_eq!(
            m.identity,
            line_identity("outside.rs", "comment", "// file-level note about tuning")
        );
        drop(dir);
    }

    #[test]
    fn ranks_are_the_flattened_display_order() {
        let (dir, conn) = seeded_conn(&[("nested.rs", NESTED_SRC)]);
        let ranked = ranked_search(vec![
            (
                crate::ranker::ResultCategory::Definition,
                vec![("nested.rs", 1, "pub fn outer_guard(a: u32) -> u32 {", 0.9)],
            ),
            (
                crate::ranker::ResultCategory::CallSite,
                vec![
                    ("nested.rs", 6, "    helper_inner(a)", 0.7),
                    ("nested.rs", 4, "        doubled + 1", 0.6),
                ],
            ),
        ]);
        let token = build_and_store_slate(&conn, "guard", &ranked, &test_feedback())
            .unwrap()
            .token;
        let (_, _, members_json) = slate_row(&conn, &token);
        let members: Vec<SlateMember> = serde_json::from_str(&members_json).unwrap();
        assert_eq!(
            members.iter().map(|m| m.rank).collect::<Vec<_>>(),
            vec![1, 2, 3],
            "1-based, group order flattened"
        );
        assert_eq!(
            members.iter().map(|m| m.line).collect::<Vec<_>>(),
            vec![1, 6, 4]
        );
        // Signals are the retained contributions as --why renders them.
        let scores = [0.9f32, 0.7, 0.6];
        for (m, &score) in members.iter().zip(scores.iter()) {
            let why = crate::output::WhyOutput::from_contributions(
                score,
                &[crate::rerank::Contribution {
                    signal: "kind",
                    value: score,
                    weight: 1.0,
                    weighted: score,
                }],
            );
            assert_eq!(m.groups.signals, why.signals);
        }
        drop(dir);
    }

    #[test]
    fn build_and_store_slate_row_is_parseable_and_carries_query() {
        let (dir, conn) = seeded_conn(&[("nested.rs", NESTED_SRC)]);
        let ranked = ranked_search(vec![(
            crate::ranker::ResultCategory::Definition,
            vec![("nested.rs", 1, "pub fn outer_guard(a: u32) -> u32 {", 0.9)],
        )]);
        let token = build_and_store_slate(&conn, "outer_guard", &ranked, &test_feedback())
            .unwrap()
            .token;
        assert_eq!(token.len(), 16);
        assert!(token.chars().all(|c| c.is_ascii_hexdigit()));
        let (query, query_class, members_json) = slate_row(&conn, &token);
        assert_eq!(query, "outer_guard");
        assert_eq!(query_class.as_deref(), Some("symbol"));
        let members: Vec<SlateMember> = serde_json::from_str(&members_json).unwrap();
        assert_eq!(members.len(), 1);
        assert!(!members[0].chosen, "nothing chosen at slate time");
        assert_eq!(members[0].score, 0.9);
        drop(dir);
    }

    #[test]
    fn prune_keeps_newest_cap() {
        let (dir, conn) = seeded_conn(&[("nested.rs", NESTED_SRC)]);
        let ranked = ranked_search(vec![(
            crate::ranker::ResultCategory::Definition,
            vec![("nested.rs", 1, "pub fn outer_guard(a: u32) -> u32 {", 0.9)],
        )]);
        let t1 = build_and_store_slate(
            &conn,
            "one",
            &ranked,
            &crate::config::FeedbackConfig {
                slate_retention: 2,
                ..test_feedback()
            },
        )
        .unwrap()
        .token;
        std::thread::sleep(std::time::Duration::from_millis(1100));
        let t2 = build_and_store_slate(
            &conn,
            "two",
            &ranked,
            &crate::config::FeedbackConfig {
                slate_retention: 2,
                ..test_feedback()
            },
        )
        .unwrap()
        .token;
        std::thread::sleep(std::time::Duration::from_millis(1100));
        let t3 = build_and_store_slate(
            &conn,
            "three",
            &ranked,
            &crate::config::FeedbackConfig {
                slate_retention: 2,
                ..test_feedback()
            },
        )
        .unwrap()
        .token;
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM feedback_slates", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 2, "retention cap enforced");
        let present = |t: &str| {
            conn.query_row(
                "SELECT COUNT(*) FROM feedback_slates WHERE token = ?1",
                [t],
                |r| r.get::<_, i64>(0),
            )
            .unwrap()
        };
        assert_eq!(present(&t1), 0, "oldest pruned");
        assert_eq!(present(&t2), 1);
        assert_eq!(present(&t3), 1);
        // Explicit prune is idempotent.
        prune_slates(&conn, 2).unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM feedback_slates", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 2);
        drop(dir);
    }

    #[test]
    fn slate_token_changes_with_nonce() {
        let members = vec![SlateMember {
            identity: "a".repeat(64),
            rank: 1,
            chosen: false,
            file: "a.rs".into(),
            line: 1,
            symbol: Some("f".into()),
            kind: Some("function".into()),
            score: 0.5,
            groups: FeatureGroups::default(),
        }];
        let t0 = slate_token("q", 42, &members, 0);
        let t0_again = slate_token("q", 42, &members, 0);
        assert_eq!(t0, t0_again, "deterministic");
        assert_ne!(slate_token("q", 42, &members, 1), t0, "nonce varies");
        assert_ne!(slate_token("q", 43, &members, 0), t0, "nanos vary");
    }

    #[test]
    fn build_and_store_slate_tokens_differ_across_calls() {
        let (dir, conn) = seeded_conn(&[("nested.rs", NESTED_SRC)]);
        let ranked = ranked_search(vec![(
            crate::ranker::ResultCategory::Definition,
            vec![("nested.rs", 1, "pub fn outer_guard(a: u32) -> u32 {", 0.9)],
        )]);
        let a = build_and_store_slate(&conn, "q", &ranked, &test_feedback())
            .unwrap()
            .token;
        let b = build_and_store_slate(&conn, "q", &ranked, &test_feedback())
            .unwrap()
            .token;
        assert_ne!(a, b, "same query back-to-back still mints distinct tokens");
        drop(dir);
    }

    // -- record_feedback + read APIs ---------------------------------------------

    /// A two-member slate over the nested fixture: rank 1 = outer_guard's
    /// definition line, rank 2 = the call-site line inside outer_guard's
    /// body (owned by outer_guard, the only span containing line 6).
    fn stored_slate(conn: &Connection) -> String {
        let ranked = ranked_search(vec![
            (
                crate::ranker::ResultCategory::Definition,
                vec![("nested.rs", 1, "pub fn outer_guard(a: u32) -> u32 {", 0.9)],
            ),
            (
                crate::ranker::ResultCategory::CallSite,
                vec![("nested.rs", 6, "    helper_inner(a)", 0.7)],
            ),
        ]);
        build_and_store_slate(conn, "guard", &ranked, &test_feedback())
            .unwrap()
            .token
    }

    #[derive(Debug)]
    struct EventRow {
        identity: String,
        class: Option<String>,
        rank: i64,
        features: String,
        session: Option<String>,
    }

    fn event_rows(conn: &Connection) -> Vec<EventRow> {
        let mut stmt = conn
            .prepare(
                "SELECT result_identity, query_class, chosen_rank, features, session \
                 FROM feedback_events ORDER BY id",
            )
            .unwrap();
        stmt.query_map([], |row| {
            Ok(EventRow {
                identity: row.get(0)?,
                class: row.get(1)?,
                rank: row.get(2)?,
                features: row.get(3)?,
                session: row.get(4)?,
            })
        })
        .unwrap()
        .flatten()
        .collect()
    }

    #[test]
    fn record_feedback_writes_one_event_per_useful_member_with_full_slate() {
        let (dir, conn) = seeded_conn(&[("nested.rs", NESTED_SRC)]);
        let token = stored_slate(&conn);
        let (_, _, members_json) = slate_row(&conn, &token);
        let members: Vec<SlateMember> = serde_json::from_str(&members_json).unwrap();
        let chosen_identity = members[1].identity.clone();
        let alt_identity = members[0].identity.clone();

        let recorded = record_feedback(&conn, &token, &["2".to_string()], "sess-1").unwrap();
        assert_eq!(recorded.recorded, 1);
        assert_eq!(recorded.events.len(), 1);
        assert_eq!(recorded.events[0].identity, chosen_identity);
        assert_eq!(recorded.events[0].rank, 2);
        assert!(
            recorded.events[0].live,
            "untouched index: everything resolves"
        );

        let rows = event_rows(&conn);
        assert_eq!(rows.len(), 1, "one event per useful member");
        let row = &rows[0];
        assert_eq!(row.identity, chosen_identity);
        assert_eq!(row.class.as_deref(), Some("symbol"));
        assert_eq!(row.rank, 2);
        assert_eq!(row.session.as_deref(), Some("sess-1"));
        let features = &row.features;

        // The features JSON carries EVERY member with chosen only on the
        // useful one (REQ-002: alternatives included).
        let parsed: SlateFeatures = serde_json::from_str(features).unwrap();
        assert_eq!(parsed.schema, 1);
        assert_eq!(parsed.slate, token);
        assert_eq!(parsed.members.len(), 2);
        let by_rank = |r: usize| parsed.members.iter().find(|m| m.rank == r).unwrap();
        assert!(by_rank(2).chosen);
        assert!(!by_rank(1).chosen);
        assert_eq!(by_rank(1).identity, alt_identity);
        drop(dir);
    }

    #[test]
    fn record_feedback_accepts_identities_and_ranks_and_multiple() {
        let (dir, conn) = seeded_conn(&[("nested.rs", NESTED_SRC)]);
        let token = stored_slate(&conn);
        let (_, _, members_json) = slate_row(&conn, &token);
        let members: Vec<SlateMember> = serde_json::from_str(&members_json).unwrap();
        let first = members[0].identity.clone();

        // Rank "2" and the rank-1 identity in one call: two events.
        record_feedback(&conn, &token, &["2".to_string(), first.clone()], "s").unwrap();
        assert_eq!(event_rows(&conn).len(), 2);

        // Same member twice (identity + rank): deduplicated to one event.
        conn.execute("DELETE FROM feedback_events", []).unwrap();
        record_feedback(&conn, &token, &["1".to_string(), first], "s").unwrap();
        assert_eq!(event_rows(&conn).len(), 1);
        drop(dir);
    }

    #[test]
    fn record_feedback_error_paths() {
        let (dir, conn) = seeded_conn(&[("nested.rs", NESTED_SRC)]);
        let token = stored_slate(&conn);

        // Unknown token: the re-search guidance.
        let err = record_feedback(&conn, "deadbeef00000000", &["1".to_string()], "s")
            .unwrap_err()
            .to_string();
        assert!(err.contains("slate not found"), "{err}");
        assert!(err.contains("re-run the search"), "{err}");

        // Empty useful.
        let err = record_feedback(&conn, &token, &[], "s")
            .unwrap_err()
            .to_string();
        assert!(err.contains("useful"), "{err}");

        // Whitespace-only useful.
        let err = record_feedback(&conn, &token, &["  ".to_string()], "s")
            .unwrap_err()
            .to_string();
        assert!(err.contains("useful"), "{err}");

        // Unknown identity and out-of-range rank.
        let err = record_feedback(&conn, &token, &["f".repeat(64)], "s")
            .unwrap_err()
            .to_string();
        assert!(err.contains("not in the slate"), "{err}");
        let err = record_feedback(&conn, &token, &["9".to_string()], "s")
            .unwrap_err()
            .to_string();
        assert!(err.contains("not in the slate"), "{err}");

        // Invalid session: empty and over-length.
        let err = record_feedback(&conn, &token, &["1".to_string()], "  ")
            .unwrap_err()
            .to_string();
        assert!(err.contains("session"), "{err}");
        let err = record_feedback(&conn, &token, &["1".to_string()], &"x".repeat(257))
            .unwrap_err()
            .to_string();
        assert!(err.contains("session"), "{err}");

        // Nothing was written by any failed call.
        assert_eq!(event_rows(&conn).len(), 0);
        drop(dir);
    }

    #[test]
    fn record_feedback_atomic_on_bad_member() {
        let (dir, conn) = seeded_conn(&[("nested.rs", NESTED_SRC)]);
        let token = stored_slate(&conn);
        // A good rank mixed with a bad identity: the whole batch fails.
        assert!(record_feedback(&conn, &token, &["1".to_string(), "b".repeat(64)], "s").is_err());
        assert_eq!(
            event_rows(&conn).len(),
            0,
            "mid-batch failure wrote nothing"
        );
        drop(dir);
    }

    #[test]
    fn distinct_sessions_counts_one_vs_many() {
        let (dir, conn) = seeded_conn(&[("nested.rs", NESTED_SRC)]);
        let token = stored_slate(&conn);
        let (_, _, members_json) = slate_row(&conn, &token);
        let first = serde_json::from_str::<Vec<SlateMember>>(&members_json).unwrap()[0]
            .identity
            .clone();

        for _ in 0..5 {
            record_feedback(&conn, &token, &["1".to_string()], "one-session").unwrap();
        }
        for n in 0..5 {
            record_feedback(&conn, &token, &["1".to_string()], &format!("s{n}")).unwrap();
        }
        // Both fixture members anchor on outer_guard, so they share one
        // identity; a never-recorded identity is the honest zero case.
        assert_eq!(distinct_sessions(&conn, &first), 6);
        assert_eq!(
            distinct_sessions(
                &conn,
                &result_identity("x.rs", "function", "never", "fn never()")
            ),
            0
        );
        drop(dir);
    }

    #[test]
    fn load_events_round_trips() {
        let (dir, conn) = seeded_conn(&[("nested.rs", NESTED_SRC)]);
        let token = stored_slate(&conn);
        let (_, _, members_json) = slate_row(&conn, &token);
        let members: Vec<SlateMember> = serde_json::from_str(&members_json).unwrap();
        record_feedback(&conn, &token, &["2".to_string()], "sess-a").unwrap();

        let events = load_events(&conn).unwrap();
        assert_eq!(events.len(), 1);
        let e = &events[0];
        assert_eq!(e.result_identity, members[1].identity);
        assert_eq!(e.query_class.as_deref(), Some("symbol"));
        assert_eq!(e.chosen_rank, 2);
        assert_eq!(e.session.as_deref(), Some("sess-a"));
        assert!(e.useful);
        assert!(e.created_at > 0);
        assert_eq!(e.features.slate, token);
        assert_eq!(e.features.members.len(), 2);
        assert!(
            e.features
                .members
                .iter()
                .find(|m| m.rank == 2)
                .unwrap()
                .chosen
        );
        drop(dir);
    }

    // -- live_identities ----------------------------------------------------------

    /// Re-index `root` after rewriting `file` to `content` (a real
    /// incremental re-index, the retirement path's actual trigger).
    fn reindex(root: &std::path::Path, file: &str, content: &str) {
        std::fs::write(root.join(file), content).unwrap();
        crate::pipeline::build_index(root, true).unwrap();
    }

    #[test]
    fn live_identities_survive_unrelated_and_body_only_edits() {
        let (dir, conn) = seeded_conn(&[("nested.rs", NESTED_SRC), ("other.rs", OTHER_SRC)]);
        let root = dir.path().to_path_buf();
        let token = stored_slate(&conn);
        let (_, _, members_json) = slate_row(&conn, &token);
        let members: Vec<SlateMember> = serde_json::from_str(&members_json).unwrap();
        let identity = members[0].identity.clone();
        let files = vec!["nested.rs".to_string(), "other.rs".to_string()];
        let queried = std::collections::HashSet::from([identity.clone()]);

        // Edit an unrelated file: nothing changes for nested.rs.
        reindex(
            &root,
            "other.rs",
            "// a new comment line\nconst FRESH: u32 = 9;\n\npub fn fresh(v: u32) -> u32 {\n    v + FRESH\n}\n",
        );
        let live = live_identities(&conn, &files, &queried);
        assert!(live.contains(&identity), "unrelated edit must not retire");

        // Body-only edit inside nested.rs: same signature, same identity.
        reindex(
            &root,
            "nested.rs",
            "pub fn outer_guard(a: u32) -> u32 {\n    fn helper_inner(x: u32) -> u32 {\n        let doubled = x * 3;\n        doubled + 1\n    }\n    helper_inner(a)\n}\n",
        );
        let live = live_identities(&conn, &files, &queried);
        assert!(live.contains(&identity), "body-only edit must not retire");
        drop(dir);
    }

    #[test]
    fn live_identities_retire_on_signature_edit_and_rename() {
        let (dir, conn) = seeded_conn(&[("nested.rs", NESTED_SRC)]);
        let root = dir.path().to_path_buf();
        let token = stored_slate(&conn);
        let (_, _, members_json) = slate_row(&conn, &token);
        let members: Vec<SlateMember> = serde_json::from_str(&members_json).unwrap();
        let files = vec!["nested.rs".to_string()];
        let queried = std::collections::HashSet::from([members[0].identity.clone()]);

        // Signature edit (rename a parameter): identity changes.
        reindex(
            &root,
            "nested.rs",
            "pub fn outer_guard(b: u32) -> u32 {\n    fn helper_inner(x: u32) -> u32 {\n        let doubled = x * 2;\n        doubled + 1\n    }\n    helper_inner(b)\n}\n",
        );
        assert!(
            !live_identities(&conn, &files, &queried).contains(&members[0].identity),
            "signature edit must retire"
        );

        // Fresh index; rename the symbol: identity changes.
        let (dir2, conn2) = seeded_conn(&[("nested.rs", NESTED_SRC)]);
        let root2 = dir2.path().to_path_buf();
        let token2 = stored_slate(&conn2);
        let (_, _, mj2) = slate_row(&conn2, &token2);
        let m2: Vec<SlateMember> = serde_json::from_str(&mj2).unwrap();
        let queried2 = std::collections::HashSet::from([m2[0].identity.clone()]);
        reindex(
            &root2,
            "nested.rs",
            "pub fn outer_renamed(a: u32) -> u32 {\n    fn helper_inner(x: u32) -> u32 {\n        let doubled = x * 2;\n        doubled + 1\n    }\n    helper_inner(a)\n}\n",
        );
        assert!(
            !live_identities(&conn2, &["nested.rs".to_string()], &queried2)
                .contains(&m2[0].identity),
            "rename must retire"
        );
        drop(dir);
        drop(dir2);
    }
    // -- descriptive feature extraction (TASK-105) -----------------------------

    fn test_feedback() -> crate::config::FeedbackConfig {
        crate::config::FeedbackConfig::default()
    }

    /// The members of the one slate in `feedback_slates`, parsed.
    fn stored_members(conn: &Connection, token: &str) -> Vec<SlateMember> {
        let (_, _, members_json) = slate_row(conn, token);
        serde_json::from_str(&members_json).unwrap()
    }

    /// The real clock the extraction itself reads, so the seeded
    /// `last_ts` values sit at fixed distances from "now".
    fn now_secs() -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64
    }

    /// A fixture with a deep file and seeded `file_churn`/`symbol_topology`
    /// rows: deterministic descriptive inputs, no git CLI needed.
    fn descriptive_conn() -> (TempDir, Connection) {
        let (dir, conn) = seeded_conn(&[
            ("nested.rs", NESTED_SRC),
            (
                "src/auth/tokens/issue.rs",
                "pub fn issue_token(user: &User) -> Token {\n    Token::sign(user.secret())\n}\n",
            ),
            (
                "src/auth/tokens/deep_helpers.rs",
                "pub fn helper_a() -> u32 {\n    1\n}\n\npub fn helper_b() -> u32 {\n    helper_a()\n}\n",
            ),
        ]);
        conn.execute(
            "INSERT INTO file_churn (file, score, last_ts, last_author, primary_author) VALUES \
             ('nested.rs', 0.0, NULL, NULL, NULL), \
             ('src/auth/tokens/issue.rs', 12.0, ?1, 'Ada', 'Ada'), \
             ('src/auth/tokens/deep_helpers.rs', 2.0, ?2, 'Bob', 'Zed')",
            rusqlite::params![now_secs() - 3600, now_secs() - 40 * 24 * 3600],
        )
        .unwrap();
        // build_index already wrote real topology rows for these symbols;
        // replace them so the degrees are the fixture's, not the graph's.
        conn.execute("DELETE FROM symbol_topology", []).unwrap();
        for (file, fan_in, fan_out) in [
            ("src/auth/tokens/issue.rs", 21i64, 0i64),
            ("src/auth/tokens/deep_helpers.rs", 4, 1),
        ] {
            conn.execute(
                "INSERT OR IGNORE INTO symbol_topology \
                 (symbol_id, hub, authority, community, fan_in, fan_out) \
                 SELECT id, 0.1, 0.2, 5, ?2, ?3 FROM symbols WHERE file = ?1",
                rusqlite::params![file, fan_in, fan_out],
            )
            .unwrap();
        }
        (dir, conn)
    }

    /// A synthetic pipeline-shaped ranked search whose SharedContext was
    /// prepared by the real pipeline over `conn` (feedback capture on).
    fn descriptive_ranked(
        conn: &Connection,
        hits: Vec<(String, u64, String, f32)>,
        query: &str,
    ) -> crate::rerank::RankedSearch {
        let results = hits
            .into_iter()
            .map(
                |(file, line, content, _score)| crate::search::SearchResult {
                    file: std::path::PathBuf::from(file),
                    line,
                    col: 1,
                    content,
                },
            )
            .collect::<Vec<_>>();
        let settings = crate::rerank::RankSettings {
            use_pipeline: true,
            feedback_capture: true,
            ..crate::rerank::RankSettings::default()
        };
        crate::rerank::rank_and_explain_classed(&results, Some(conn), query, &settings)
    }

    #[test]
    fn path_features_emit_one_per_ancestor_level() {
        let (dir, conn) = descriptive_conn();
        let ranked = descriptive_ranked(
            &conn,
            vec![(
                "src/auth/tokens/issue.rs".to_string(),
                1,
                "pub fn issue_token(user: &User) -> Token {".to_string(),
                0.9,
            )],
            "issue_token",
        );
        let token = build_and_store_slate(&conn, "issue_token", &ranked, &test_feedback())
            .unwrap()
            .token;
        let members = stored_members(&conn, &token);
        let path = &members[0].groups.path;
        assert_eq!(path.get("src").map(String::as_str), Some("1"));
        assert_eq!(path.get("src/auth").map(String::as_str), Some("1"));
        assert_eq!(path.get("src/auth/tokens").map(String::as_str), Some("1"));
        assert_eq!(
            path.get("depth").map(String::as_str),
            Some("mid"),
            "{path:?}"
        );
        assert_eq!(path.get("class").map(String::as_str), Some("ordinary"));
        assert_eq!(path.get("lang").map(String::as_str), Some("Rust"));
        assert_eq!(
            path.len(),
            6,
            "three ancestors + depth/class/lang: {path:?}"
        );

        // A root-level file has no ancestor features at all.
        let ranked = descriptive_ranked(
            &conn,
            vec![(
                "nested.rs".to_string(),
                1,
                "pub fn outer_guard(a: u32) -> u32 {".to_string(),
                0.9,
            )],
            "outer_guard",
        );
        let token = build_and_store_slate(&conn, "outer_guard", &ranked, &test_feedback())
            .unwrap()
            .token;
        let path = &stored_members(&conn, &token)[0].groups.path;
        assert!(
            !path
                .keys()
                .any(|k| !["class", "depth", "lang"].contains(&k.as_str())),
            "{path:?}"
        );
        assert_eq!(path.get("depth").map(String::as_str), Some("shallow"));
        drop(dir);
    }

    #[test]
    fn line_anchored_members_get_path_and_match_but_not_symbol_groups() {
        let (dir, conn) = descriptive_conn();
        // A line beyond every symbol span of the file — line-anchored.
        let ranked = descriptive_ranked(
            &conn,
            vec![(
                "src/auth/tokens/issue.rs".to_string(),
                99,
                "// a trailing note no symbol owns".to_string(),
                0.5,
            )],
            "never_matching_query_zzz",
        );
        let token = build_and_store_slate(&conn, "zzz", &ranked, &test_feedback())
            .unwrap()
            .token;
        let groups = &stored_members(&conn, &token)[0].groups;
        assert!(groups.symbol.is_empty(), "no owning symbol: {groups:?}");
        assert!(groups.graph.is_empty());
        assert!(
            !groups.path.is_empty(),
            "path features come from the canonical key"
        );
        assert!(!groups.match_.is_empty());
        drop(dir);
    }

    #[test]
    fn body_size_bucket_edges() {
        // 7 tiny / 8 small / 29 small / 30 medium / 99 medium / 100 large
        // / 399 large / 400 huge.
        assert_eq!(body_size_bucket(7), "tiny");
        assert_eq!(body_size_bucket(8), "small");
        assert_eq!(body_size_bucket(29), "small");
        assert_eq!(body_size_bucket(30), "medium");
        assert_eq!(body_size_bucket(99), "medium");
        assert_eq!(body_size_bucket(100), "large");
        assert_eq!(body_size_bucket(399), "large");
        assert_eq!(body_size_bucket(400), "huge");
    }

    #[test]
    fn fan_depth_recency_churn_co_change_coverage_edges() {
        for (degree, label) in [
            (0u32, "zero"),
            (4, "low"),
            (5, "medium"),
            (19, "medium"),
            (20, "high"),
        ] {
            assert_eq!(fan_bucket(degree), label);
        }
        for (ancestors, label) in [
            (0usize, "shallow"),
            (2, "shallow"),
            (3, "mid"),
            (4, "mid"),
            (5, "deep"),
        ] {
            assert_eq!(depth_bucket(ancestors), label);
        }
        let hour = 3600;
        for (age, label) in [
            (23 * hour, "hours"),
            (25 * hour, "days"),
            (6 * 24 * hour, "days"),
            (8 * 24 * hour, "weeks"),
            (29 * 24 * hour, "weeks"),
            (31 * 24 * hour, "months"),
            (364 * 24 * hour, "months"),
            (366 * 24 * hour, "ancient"),
        ] {
            assert_eq!(recency_bucket(age), label);
        }
        for (score, label) in [
            (0.0f32, "zero"),
            (2.9, "low"),
            (3.0, "medium"),
            (9.9, "medium"),
            (10.0, "high"),
        ] {
            assert_eq!(churn_bucket(score), label, "churn");
            assert_eq!(
                co_change_bucket(score),
                if score == 0.0 {
                    "none"
                } else if score < 3.0 {
                    "weak"
                } else {
                    "strong"
                },
                "co_change mirrors the churn scale"
            );
        }
        assert_eq!(coverage_bucket(0, 0), "none");
        assert_eq!(coverage_bucket(0, 3), "none");
        assert_eq!(coverage_bucket(1, 3), "some");
        assert_eq!(coverage_bucket(2, 3), "most");
        assert_eq!(coverage_bucket(3, 3), "all");
    }

    #[test]
    fn continuous_properties_land_as_labels_not_raw_values() {
        let (dir, conn) = descriptive_conn();
        let ranked = descriptive_ranked(
            &conn,
            vec![
                (
                    "src/auth/tokens/issue.rs".to_string(),
                    1,
                    "pub fn issue_token(user: &User) -> Token {".to_string(),
                    0.9,
                ),
                (
                    "src/auth/tokens/deep_helpers.rs".to_string(),
                    1,
                    "pub fn helper_a() -> u32 {".to_string(),
                    0.8,
                ),
            ],
            "helper_a",
        );
        let token = build_and_store_slate(&conn, "helper_a", &ranked, &test_feedback())
            .unwrap()
            .token;
        let members = stored_members(&conn, &token);

        let issue = &members
            .iter()
            .find(|m| m.file.ends_with("issue.rs"))
            .unwrap()
            .groups;
        // issue.rs: churn 12.0 -> high; last_ts an hour ago -> hours;
        // fan_in 21 -> high; fan_out 0 -> zero; body 3 lines -> tiny.
        assert_eq!(issue.history.get("churn").map(String::as_str), Some("high"));
        assert_eq!(
            issue.history.get("recency").map(String::as_str),
            Some("hours")
        );
        assert_eq!(issue.graph.get("fan_in").map(String::as_str), Some("high"));
        assert_eq!(issue.graph.get("fan_out").map(String::as_str), Some("zero"));
        assert_eq!(
            issue.symbol.get("body_size").map(String::as_str),
            Some("tiny")
        );
        assert_eq!(issue.author.get("primary").map(String::as_str), Some("Ada"));
        assert_eq!(
            issue.author.get("last_touched_by").map(String::as_str),
            Some("Ada")
        );

        let helper = &members
            .iter()
            .find(|m| m.file.ends_with("deep_helpers.rs"))
            .unwrap()
            .groups;
        // deep_helpers.rs: churn 2.0 -> low; 40 days old -> months;
        // fan_in 4 -> low; fan_out 1 -> low; primary Zed, last Bob.
        assert_eq!(helper.history.get("churn").map(String::as_str), Some("low"));
        assert_eq!(
            helper.history.get("recency").map(String::as_str),
            Some("months")
        );
        assert_eq!(helper.graph.get("fan_in").map(String::as_str), Some("low"));
        assert_eq!(helper.graph.get("fan_out").map(String::as_str), Some("low"));
        assert_eq!(
            helper.author.get("primary").map(String::as_str),
            Some("Zed")
        );
        assert_eq!(
            helper.author.get("last_touched_by").map(String::as_str),
            Some("Bob")
        );

        // No raw number appears anywhere in the labeled groups.
        for member in &members {
            for (name, value) in member
                .groups
                .history
                .iter()
                .chain(member.groups.graph.iter())
            {
                assert!(
                    !value.chars().any(|c| c.is_ascii_digit()),
                    "{name} leaked a raw value: {value}"
                );
            }
            let body = member.groups.symbol.get("body_size").unwrap();
            assert!(!body.chars().any(|c| c.is_ascii_digit()));
        }
        drop(dir);
    }

    #[test]
    fn unmined_history_omits_the_whole_history_group() {
        let (dir, conn) = seeded_conn(&[("nested.rs", NESTED_SRC)]);
        let ranked = descriptive_ranked(
            &conn,
            vec![(
                "nested.rs".to_string(),
                1,
                "pub fn outer_guard(a: u32) -> u32 {".to_string(),
                0.9,
            )],
            "outer_guard",
        );
        let token = build_and_store_slate(&conn, "outer_guard", &ranked, &test_feedback())
            .unwrap()
            .token;
        let member = &stored_members(&conn, &token)[0];
        assert!(member.groups.history.is_empty(), "no mining, no group");
        assert!(member.groups.author.is_empty());
        let raw = slate_row(&conn, &token).2;
        assert!(!raw.contains("\"history\""), "absent, not defaulted: {raw}");
        assert!(!raw.contains("\"author\""), "{raw}");
        drop(dir);
    }

    #[test]
    fn missing_topology_row_omits_graph_keys_only() {
        let (dir, conn) = descriptive_conn();
        conn.execute("DELETE FROM symbol_topology", []).unwrap();
        let ranked = descriptive_ranked(
            &conn,
            vec![(
                "src/auth/tokens/issue.rs".to_string(),
                1,
                "pub fn issue_token(user: &User) -> Token {".to_string(),
                0.9,
            )],
            "issue_token",
        );
        let token = build_and_store_slate(&conn, "issue_token", &ranked, &test_feedback())
            .unwrap()
            .token;
        let groups = &stored_members(&conn, &token)[0].groups;
        assert!(groups.graph.is_empty(), "no topology row, no graph keys");
        assert!(!groups.history.is_empty(), "churn rows still present");
        drop(dir);
    }

    #[test]
    fn old_shape_feature_groups_json_still_deserializes() {
        let raw = r#"{"signals":[{"signal":"kind","value":1.0,"weight":1.0,"weighted":1.0}]}"#;
        let groups: FeatureGroups = serde_json::from_str(raw).unwrap();
        assert_eq!(groups.signals.len(), 1);
        assert!(groups.path.is_empty());
        assert!(groups.context.is_empty());
        // And the new shape round-trips.
        let round = serde_json::to_string(&groups).unwrap();
        let back: FeatureGroups = serde_json::from_str(&round).unwrap();
        assert_eq!(groups, back);
    }

    #[test]
    fn overflow_cap_collapses_beyond_32_distinct_values() {
        let (dir, conn) = seeded_conn(&[
            ("nested.rs", NESTED_SRC),
            (
                "src/auth/tokens/issue.rs",
                "pub fn issue_token(user: &User) -> Token {\n    Token::sign(user.secret())\n}\n",
            ),
        ]);
        // 40 files under 40 distinct directories, 40 distinct authors.
        let mut hits = vec![(
            "nested.rs".to_string(),
            1u64,
            "pub fn outer_guard(a: u32) -> u32 {".to_string(),
            0.9f32,
        )];
        for i in 0..40 {
            let file = format!("gen{i:02}/mod_file.rs");
            std::fs::create_dir_all(dir.path().join(&file).parent().unwrap()).unwrap();
            std::fs::write(
                dir.path().join(&file),
                format!("pub fn generated_{i}() -> u32 {{ {i} }}\n"),
            )
            .unwrap();
            hits.push((
                file.clone(),
                1,
                format!("pub fn generated_{i}() -> u32 {{ {i} }}"),
                0.5,
            ));
        }
        crate::pipeline::build_index(dir.path(), true).unwrap();
        for i in 0..40 {
            conn.execute(
                "INSERT INTO file_churn (file, score, last_ts, last_author, primary_author) \
                 VALUES (?1, 1.0, 1, ?2, ?2)",
                rusqlite::params![format!("gen{i:02}/mod_file.rs"), format!("Author{i:02}")],
            )
            .unwrap();
        }
        let ranked = descriptive_ranked(&conn, hits, "generated");
        let token = build_and_store_slate(&conn, "generated", &ranked, &test_feedback())
            .unwrap()
            .token;
        let members = stored_members(&conn, &token);

        let primaries: Vec<&str> = members
            .iter()
            .filter_map(|m| m.groups.author.get("primary").map(String::as_str))
            .collect();
        let kept = primaries.iter().filter(|p| **p != OVERFLOW_LABEL).count();
        let overflowed = primaries.iter().filter(|p| **p == OVERFLOW_LABEL).count();
        assert_eq!(
            kept, CATEGORICAL_CAP,
            "exactly the cap keeps its label: kept {kept}, overflowed {overflowed}"
        );
        assert!(overflowed >= 40 - CATEGORICAL_CAP, "{overflowed}");

        // The ancestor family caps as one pool: the 40 distinct gen dirs
        // plus the deeper fixtures' dirs collapse so at most 32 distinct
        // ancestor keys survive, wearing one shared __overflow__ key.
        let with_overflow_ancestor = members
            .iter()
            .filter(|m| m.groups.path.contains_key(OVERFLOW_LABEL))
            .count();
        assert!(
            with_overflow_ancestor >= 40 - CATEGORICAL_CAP,
            "{with_overflow_ancestor}"
        );
        let ancestors: std::collections::BTreeSet<&str> = members
            .iter()
            .flat_map(|m| {
                m.groups
                    .path
                    .keys()
                    .filter(|k| !PATH_SCALARS.contains(&k.as_str()) && k.as_str() != OVERFLOW_LABEL)
            })
            .map(String::as_str)
            .collect();
        assert!(
            ancestors.len() <= CATEGORICAL_CAP,
            "kept ancestor keys obey the cap: {}",
            ancestors.len()
        );
        drop(dir);
    }

    #[test]
    fn extraction_is_deterministic_for_identical_inputs() {
        let (dir, conn) = descriptive_conn();
        let build = || {
            let ranked = descriptive_ranked(
                &conn,
                vec![
                    (
                        "src/auth/tokens/issue.rs".to_string(),
                        1,
                        "pub fn issue_token(user: &User) -> Token {".to_string(),
                        0.9,
                    ),
                    (
                        "src/auth/tokens/deep_helpers.rs".to_string(),
                        1,
                        "pub fn helper_a() -> u32 {".to_string(),
                        0.8,
                    ),
                ],
                "issue_token",
            );
            build_and_store_slate(&conn, "issue_token", &ranked, &test_feedback())
                .unwrap()
                .token
        };
        let a = build();
        let b = build();
        let members_a = stored_members(&conn, &a);
        let members_b = stored_members(&conn, &b);
        let groups_a =
            serde_json::to_string(&members_a.iter().map(|m| &m.groups).collect::<Vec<_>>())
                .unwrap();
        let groups_b =
            serde_json::to_string(&members_b.iter().map(|m| &m.groups).collect::<Vec<_>>())
                .unwrap();
        assert_eq!(
            groups_a, groups_b,
            "identical inputs, identical groups JSON"
        );
        drop(dir);
    }

    #[test]
    fn author_switch_off_removes_author_and_touches_nothing_else() {
        let (dir, conn) = descriptive_conn();
        let hits = vec![
            (
                "src/auth/tokens/issue.rs".to_string(),
                1u64,
                "pub fn issue_token(user: &User) -> Token {".to_string(),
                0.9f32,
            ),
            (
                "src/auth/tokens/deep_helpers.rs".to_string(),
                1,
                "pub fn helper_a() -> u32 {".to_string(),
                0.8,
            ),
        ];
        let ranked = descriptive_ranked(&conn, hits, "issue_token");
        let on = build_and_store_slate(
            &conn,
            "issue_token",
            &ranked,
            &crate::config::FeedbackConfig {
                author_features: true,
                ..test_feedback()
            },
        )
        .unwrap()
        .token;
        let off = build_and_store_slate(
            &conn,
            "issue_token",
            &ranked,
            &crate::config::FeedbackConfig {
                author_features: false,
                ..test_feedback()
            },
        )
        .unwrap()
        .token;
        let on_members = stored_members(&conn, &on);
        let off_members = stored_members(&conn, &off);
        let raw_off = slate_row(&conn, &off).2;
        assert!(
            !raw_off.contains("\"author\""),
            "key absent in JSON: {raw_off}"
        );
        for (on_m, off_m) in on_members.iter().zip(off_members.iter()) {
            assert!(!on_m.groups.author.is_empty(), "switch on records authors");
            assert!(
                off_m.groups.author.is_empty(),
                "switch off never builds the group"
            );
            // Every other group is byte-identical.
            assert_eq!(on_m.groups.path, off_m.groups.path);
            assert_eq!(on_m.groups.symbol, off_m.groups.symbol);
            assert_eq!(on_m.groups.match_, off_m.groups.match_);
            assert_eq!(on_m.groups.graph, off_m.groups.graph);
            assert_eq!(on_m.groups.history, off_m.groups.history);
            assert_eq!(on_m.groups.context, off_m.groups.context);
            assert_eq!(on_m.groups.signals, off_m.groups.signals);
        }
        drop(dir);
    }
    // -- context-relative features (TASK-105, PRD-FB-REQ-027) ------------------

    use crate::rerank::{SharedContext, WorkingContext};

    /// A pipeline-shaped ranked search carrying a caller-built shared
    /// context — the seam the context-relative features read.
    fn ranked_with_context(
        hits: Vec<(&str, u64, &str, f32)>,
        ctx: SharedContext,
    ) -> crate::rerank::RankedSearch {
        let ranked = ranked_search(vec![(crate::ranker::ResultCategory::Definition, hits)]);
        crate::rerank::RankedSearch {
            context: ctx,
            ..ranked
        }
    }

    fn symbol_id(conn: &Connection, name: &str) -> i64 {
        conn.query_row("SELECT id FROM symbols WHERE name = ?1", [name], |r| {
            r.get(0)
        })
        .unwrap()
    }

    #[test]
    fn context_features_over_a_populated_working_context() {
        let (dir, conn) = seeded_conn(&[("nested.rs", NESTED_SRC), ("sub/other.rs", OTHER_SRC)]);
        let hint_id = symbol_id(&conn, "outer_guard");
        let other_id = symbol_id(&conn, "other_entry");
        let ctx = SharedContext {
            working: WorkingContext {
                hint: Some("nested.rs".to_string()),
                path: Some("nested.rs".to_string()),
                community: Some(7),
                distances: HashMap::from([(hint_id, 1i64), (other_id, 2)]),
                partners: HashMap::from([
                    ("nested.rs".to_string(), 4.0f32),
                    ("sub/other.rs".to_string(), 1.5),
                ]),
            },
            topology: crate::rerank::TopologyContext {
                communities: HashMap::from([
                    (("nested.rs".to_string(), 1u64), 7i64),
                    (("sub/other.rs".to_string(), 1u64), 9),
                ]),
                ..Default::default()
            },
            ..Default::default()
        };
        let ranked = ranked_with_context(
            vec![
                // The hint file itself: same file, same (root) directory,
                // same community, distance 1, coupled 4.0.
                ("nested.rs", 1, "pub fn outer_guard(a: u32) -> u32 {", 0.9),
                // A sibling in a subdirectory: different file, different
                // directory, different community, distance 2, coupled 1.5.
                (
                    "sub/other.rs",
                    1,
                    "pub fn other_entry(x: i64) -> i64 {",
                    0.7,
                ),
                // Line-anchored in the hint file.
                ("nested.rs", 99, "// outside every span", 0.5),
            ],
            ctx,
        );
        let token = build_and_store_slate(&conn, "guard", &ranked, &test_feedback())
            .unwrap()
            .token;
        let members = stored_members(&conn, &token);

        let first = &members[0].groups.context;
        assert_eq!(first.get("same_file").map(String::as_str), Some("yes"));
        assert_eq!(first.get("same_directory").map(String::as_str), Some("yes"));
        assert_eq!(first.get("same_community").map(String::as_str), Some("yes"));
        assert_eq!(
            first.get("import_distance").map(String::as_str),
            Some("direct")
        );
        assert_eq!(first.get("co_change").map(String::as_str), Some("strong"));

        let second = &members[1].groups.context;
        assert_eq!(second.get("same_file").map(String::as_str), Some("no"));
        assert_eq!(second.get("same_directory").map(String::as_str), Some("no"));
        assert_eq!(second.get("same_community").map(String::as_str), Some("no"));
        assert_eq!(
            second.get("import_distance").map(String::as_str),
            Some("transitive")
        );
        assert_eq!(second.get("co_change").map(String::as_str), Some("weak"));

        // Line-anchored: import_distance is omitted (no owning symbol id),
        // the string-computable keys stay.
        let third = &members[2].groups.context;
        assert!(!third.contains_key("import_distance"));
        assert_eq!(third.get("same_file").map(String::as_str), Some("yes"));
        drop(dir);
    }

    #[test]
    fn unresolvable_hint_still_emits_string_computable_keys() {
        let (dir, conn) = seeded_conn(&[("nested.rs", NESTED_SRC)]);
        let ctx = SharedContext {
            working: WorkingContext {
                hint: Some("src/gone/missing.rs".to_string()),
                ..Default::default()
            },
            ..Default::default()
        };
        let ranked = ranked_with_context(
            vec![("nested.rs", 1, "pub fn outer_guard(a: u32) -> u32 {", 0.9)],
            ctx,
        );
        let token = build_and_store_slate(&conn, "guard", &ranked, &test_feedback())
            .unwrap()
            .token;
        let groups = &stored_members(&conn, &token)[0].groups;
        assert!(
            !groups.context.is_empty(),
            "the group is present: {groups:?}"
        );
        assert_eq!(
            groups.context.get("same_file").map(String::as_str),
            Some("no")
        );
        assert_eq!(
            groups.context.get("same_directory").map(String::as_str),
            Some("no")
        );
        assert_eq!(
            groups.context.get("co_change").map(String::as_str),
            Some("none")
        );
        assert!(
            !groups.context.contains_key("same_community"),
            "unknown communities are omitted, not defaulted"
        );
        drop(dir);
    }

    #[test]
    fn absolute_hint_matches_canonical_member_via_suffix_equivalence() {
        let (dir, conn) = seeded_conn(&[("nested.rs", NESTED_SRC)]);
        let absolute = dir.path().join("nested.rs");
        let ctx = SharedContext {
            working: WorkingContext {
                hint: Some(absolute.to_string_lossy().into_owned()),
                path: Some("nested.rs".to_string()),
                ..Default::default()
            },
            ..Default::default()
        };
        let ranked = ranked_with_context(
            vec![("nested.rs", 1, "pub fn outer_guard(a: u32) -> u32 {", 0.9)],
            ctx,
        );
        let token = build_and_store_slate(&conn, "guard", &ranked, &test_feedback())
            .unwrap()
            .token;
        assert_eq!(
            stored_members(&conn, &token)[0]
                .groups
                .context
                .get("same_file")
                .map(String::as_str),
            Some("yes")
        );
        drop(dir);
    }

    #[test]
    fn no_hint_means_no_context_key_anywhere() {
        let (dir, conn) = descriptive_conn();
        let ranked = descriptive_ranked(
            &conn,
            vec![(
                "src/auth/tokens/issue.rs".to_string(),
                1,
                "pub fn issue_token(user: &User) -> Token {".to_string(),
                0.9,
            )],
            "issue_token",
        );
        assert!(ranked.context.working_hint().is_none(), "no hint anywhere");
        let token = build_and_store_slate(&conn, "issue_token", &ranked, &test_feedback())
            .unwrap()
            .token;
        let raw = slate_row(&conn, &token).2;
        assert!(
            !raw.contains("\"context\""),
            "absent rather than defaulted: {raw}"
        );
        drop(dir);
    }

    #[test]
    fn weak_co_change_and_unreachable_distance_labels() {
        let (dir, conn) = seeded_conn(&[("nested.rs", NESTED_SRC)]);
        let ctx = SharedContext {
            working: WorkingContext {
                hint: Some("elsewhere.rs".to_string()),
                path: Some("elsewhere.rs".to_string()),
                partners: HashMap::from([("nested.rs".to_string(), 1.5f32)]),
                ..Default::default()
            },
            ..Default::default()
        };
        let ranked = ranked_with_context(
            vec![("nested.rs", 1, "pub fn outer_guard(a: u32) -> u32 {", 0.9)],
            ctx,
        );
        let token = build_and_store_slate(&conn, "guard", &ranked, &test_feedback())
            .unwrap()
            .token;
        let context = &stored_members(&conn, &token)[0].groups.context;
        assert_eq!(
            context.get("import_distance").map(String::as_str),
            Some("unreachable")
        );
        assert_eq!(context.get("co_change").map(String::as_str), Some("weak"));
        drop(dir);
    }
}
