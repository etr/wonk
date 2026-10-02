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

/// One `symbols` row, bulk-loaded for span resolution.
struct SymbolRow {
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

/// Bulk-load the symbol rows of `files` (one bounded query per search).
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
            .query_map(rusqlite::params_from_iter(chunk.iter()), |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    SymbolRow {
                        line: row.get(1)?,
                        end_line: row.get(2)?,
                        name: row.get(3)?,
                        kind: row.get(4)?,
                        signature: row.get(5)?,
                    },
                ))
            })?
            .collect::<rusqlite::Result<Vec<(String, SymbolRow)>>>()?;
        for (file, symbol) in rows {
            map.entry(file).or_default().push(symbol);
        }
    }
    Ok(map)
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
                result_identity(
                    &file,
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

/// Build the slate for `ranked` and persist it as one `feedback_slates`
/// row, pruned to `retention`, in one transaction. Returns the echoed
/// token. The caller gates this on `[feedback] enabled` AND the pipeline
/// having run — a legacy-path slate carries no contributions to learn
/// from.
pub fn build_and_store_slate(
    conn: &Connection,
    query: &str,
    ranked: &crate::rerank::RankedSearch,
    retention: usize,
) -> Result<String> {
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
    Ok(token)
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
        let token = build_and_store_slate(&conn, "doubled", &ranked, 64).unwrap();
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
        let token = build_and_store_slate(&conn, "tuning", &ranked, 64).unwrap();
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
        let token = build_and_store_slate(&conn, "guard", &ranked, 64).unwrap();
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
        let token = build_and_store_slate(&conn, "outer_guard", &ranked, 64).unwrap();
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
        let t1 = build_and_store_slate(&conn, "one", &ranked, 2).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(1100));
        let t2 = build_and_store_slate(&conn, "two", &ranked, 2).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(1100));
        let t3 = build_and_store_slate(&conn, "three", &ranked, 2).unwrap();
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
        let a = build_and_store_slate(&conn, "q", &ranked, 64).unwrap();
        let b = build_and_store_slate(&conn, "q", &ranked, 64).unwrap();
        assert_ne!(a, b, "same query back-to-back still mints distinct tokens");
        drop(dir);
    }
}
