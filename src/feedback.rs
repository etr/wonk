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

use std::collections::HashMap;
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
/// `ContributionOutput`, the same serialized type. TASK-105 extends this
/// struct with further `#[serde(default)]` sibling groups; old rows stay
/// deserializable.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct FeatureGroups {
    /// One entry per active builtin signal, in registry order.
    #[serde(default)]
    pub signals: Vec<crate::output::ContributionOutput>,
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
/// result path the caller saw (which may be absolute).
struct SymbolRow {
    file: String,
    line: i64,
    end_line: Option<i64>,
    name: String,
    kind: String,
    signature: Option<String>,
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
            "SELECT file, line, end_line, name, kind, signature FROM symbols \
             WHERE file IN ({placeholders})"
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
        let sql = "SELECT file, line, end_line, name, kind, signature FROM symbols \
                   WHERE ?1 LIKE '%' || file";
        let mut stmt = conn.prepare(sql)?;
        let rows = stmt
            .query_map([file], symbol_row)?
            .collect::<rusqlite::Result<Vec<SymbolRow>>>()?;
        let matched: Vec<SymbolRow> = rows
            .into_iter()
            .filter(|r| is_path_suffix(file, &r.file))
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
        file: row.get(0)?,
        line: row.get(1)?,
        end_line: row.get(2)?,
        name: row.get(3)?,
        kind: row.get(4)?,
        signature: row.get(5)?,
    })
}

/// Whether `suffix` is `path`'s tail at a path-separator boundary (or
/// equal to it).
fn is_path_suffix(path: &str, suffix: &str) -> bool {
    path == suffix
        || (path.len() > suffix.len()
            && path.ends_with(suffix)
            && path[..path.len() - suffix.len()].ends_with('/'))
}

/// SQLite's default host-parameter limit; chunking keeps the IN-list
/// bounded no matter how many files a result set names.
const SQL_VAR_LIMIT: usize = 500;

/// Build the slate members for a ranked search: flattened display order,
/// each member identity-anchored on its owning symbol's DB row (or the
/// matched line when no symbol owns it) and carrying the retained signal
/// contributions verbatim.
fn build_members(
    conn: &Connection,
    ranked: &crate::rerank::RankedSearch,
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
    let mut members = Vec::with_capacity(flat.len());
    for (idx, item) in flat.iter().enumerate() {
        let result = &item.classified.result;
        let file = result.file.to_string_lossy().into_owned();
        let rows = symbols.get(&file).map(Vec::as_slice).unwrap_or(&[]);
        let (identity, symbol, kind) = match owning_symbol(rows, result.line) {
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
            ),
            None => (
                line_identity(
                    &file,
                    &item.classified.category.to_string(),
                    &result.content,
                ),
                None,
                None,
            ),
        };
        members.push(SlateMember {
            identity,
            rank: idx + 1,
            chosen: false,
            file,
            line: result.line,
            symbol,
            kind,
            score: item.score,
            groups: FeatureGroups {
                signals: crate::output::WhyOutput::from_contributions(
                    item.score,
                    &item.contributions,
                )
                .signals,
            },
        });
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
/// row, pruned to `retention`, in one transaction. Returns the echoed
/// token with the members. The caller gates this on `[feedback] enabled`
/// AND the pipeline having run — a legacy-path slate carries no
/// contributions to learn from.
pub fn build_and_store_slate(
    conn: &Connection,
    query: &str,
    ranked: &crate::rerank::RankedSearch,
    retention: usize,
) -> Result<StoredSlate> {
    let members = build_members(conn, ranked)?;
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
    prune_slates(&tx, retention)?;
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
        let token = build_and_store_slate(&conn, "doubled", &ranked, 64)
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
        let token = build_and_store_slate(&conn, "tuning", &ranked, 64)
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
        let token = build_and_store_slate(&conn, "guard", &ranked, 64)
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
        let token = build_and_store_slate(&conn, "outer_guard", &ranked, 64)
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
        let t1 = build_and_store_slate(&conn, "one", &ranked, 2)
            .unwrap()
            .token;
        std::thread::sleep(std::time::Duration::from_millis(1100));
        let t2 = build_and_store_slate(&conn, "two", &ranked, 2)
            .unwrap()
            .token;
        std::thread::sleep(std::time::Duration::from_millis(1100));
        let t3 = build_and_store_slate(&conn, "three", &ranked, 2)
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
        let a = build_and_store_slate(&conn, "q", &ranked, 64)
            .unwrap()
            .token;
        let b = build_and_store_slate(&conn, "q", &ranked, 64)
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
        build_and_store_slate(conn, "guard", &ranked, 64)
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
}
