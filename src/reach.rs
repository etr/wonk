//! Precomputed upstream reachability (TASK-080, DR-034).
//!
//! Materializes, per eligible symbol name, the bounded-depth set of symbols
//! that (transitively) call it — the same set `blast` discovers by live BFS.
//! The build mirrors `blast::analyze_blast`'s upstream traversal rule for
//! rule (name-keyed BFS over `references.caller_id`, type-edge children at
//! depth 1), so the table is equivalent to the BFS by construction and the
//! equivalence suite only guards against drift.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::Path;
use std::str::FromStr;

use anyhow::{Context, Result};
use rusqlite::Connection;

use crate::types::{BlastAffectedSymbol, SymbolKind};

/// Default per-source fan-out cap on recorded targets (PRD-REACH-REQ-009).
/// Bounds total reach-set size per source so pathological fan-out cannot
/// blow up the table.
pub const DEFAULT_MAX_TARGETS_PER_SOURCE: usize = 500;

/// `reach_meta` key holding the depth the table was built to.
pub(crate) const META_BUILT_DEPTH: &str = "built_depth";

/// `reach_meta` key whose presence marks the table stale (PRD-REACH-REQ-007).
/// Set by incremental file updates until TASK-081 recomputes affected rows.
pub(crate) const META_STALE: &str = "stale";

/// Whether a symbol kind can be a precomputation target (PRD-REACH-REQ-010).
///
/// Files, imports, and parameters are never symbols, so the exclusion list
/// reduces to `Module` — the only structural kind (impl blocks, mod items,
/// Ruby modules). Functions, methods, types, interfaces, constants, and
/// variables are all eligible.
pub fn is_reach_seed(kind: &SymbolKind) -> bool {
    !matches!(kind, SymbolKind::Module)
}

/// Edge eligibility shared by the precomputed build and the live BFS (AR-021).
///
/// One predicate, two callers: `analyze_blast` applies it to every candidate
/// it considers and `build_reach` applies it to every candidate it records,
/// so a change here moves both paths in lockstep.
#[derive(Debug, Clone, PartialEq)]
pub struct EdgeFilter {
    /// Minimum edge confidence (edges below this are ineligible).
    pub min_confidence: f64,
    /// Whether symbols discovered in test files are eligible.
    pub include_tests: bool,
}

impl Default for EdgeFilter {
    fn default() -> Self {
        Self {
            min_confidence: 0.0,
            include_tests: false,
        }
    }
}

/// The shared edge-eligibility predicate (AR-021).
///
/// An edge discovering a symbol in `discovered_file` with `confidence` is
/// eligible iff it passes the confidence floor and (unless tests are
/// included) does not land in a test file.
pub fn edge_eligible(discovered_file: &str, confidence: f64, filter: &EdgeFilter) -> bool {
    if confidence < filter.min_confidence {
        return false;
    }
    if !filter.include_tests && crate::ranker::is_test_file(Path::new(discovered_file)) {
        return false;
    }
    true
}

/// Options for a full reach build.
#[derive(Debug, Clone)]
pub struct ReachBuildOptions {
    /// Depth to materialize (clamped by callers to `blast::MAX_DEPTH`).
    pub depth: usize,
    /// Per-source fan-out cap on recorded targets
    /// (PRD-REACH-REQ-009). No config key; use
    /// [`DEFAULT_MAX_TARGETS_PER_SOURCE`] unless measuring.
    pub max_targets: usize,
}

impl Default for ReachBuildOptions {
    fn default() -> Self {
        Self {
            depth: crate::blast::DEFAULT_DEPTH,
            max_targets: DEFAULT_MAX_TARGETS_PER_SOURCE,
        }
    }
}

/// Statistics from a completed reach build.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReachBuildStats {
    /// Names with at least one eligible (non-Module) symbol.
    pub sources: usize,
    /// Rows written to `reach`.
    pub rows: usize,
    /// Sources whose traversal hit the fan-out cap.
    pub truncated_sources: usize,
}

/// Mark the reach table stale: the next qualifying lookup falls back to BFS
/// (PRD-REACH-REQ-007) until a full rebuild clears the marker. Presence of
/// the `stale` key is the marker; there is no value to read.
pub fn mark_stale(tx: &rusqlite::Transaction) -> Result<()> {
    crate::db::ensure_reach_table(tx)?;
    tx.execute(
        "INSERT OR REPLACE INTO reach_meta (key, value) VALUES (?1, '1')",
        rusqlite::params![META_STALE],
    )
    .context("marking reach table stale")?;
    Ok(())
}

/// A symbol row loaded for the build.
#[derive(Debug, Clone)]
struct LoadedSymbol {
    id: i64,
    name: String,
    kind: SymbolKind,
    file: String,
    line: i64,
}

/// In-memory graph the per-name BFS runs over, loaded in three queries.
struct ReachGraph {
    /// All symbols (any kind — Modules can be targets), ordered by id.
    symbols: Vec<LoadedSymbol>,
    /// Eligibility (non-Module) per symbol position.
    eligible: Vec<bool>,
    /// name -> positions into `symbols` (all kinds).
    by_name: HashMap<String, Vec<usize>>,
    /// callee name -> (caller position, confidence) for every reference
    /// with a resolved caller_id.
    refs_by_name: HashMap<String, Vec<(usize, f64)>>,
    /// parent name -> child positions (union over same-named parents).
    children_by_parent_name: HashMap<String, Vec<usize>>,
}

impl ReachGraph {
    fn load(conn: &Connection) -> Result<Self> {
        let mut symbols = Vec::new();
        {
            let mut stmt =
                conn.prepare("SELECT id, name, kind, file, line FROM symbols ORDER BY id")?;
            let rows = stmt.query_map([], |row| {
                Ok(LoadedSymbol {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    kind: SymbolKind::from_str(&row.get::<_, String>(2)?)
                        .unwrap_or(SymbolKind::Function),
                    file: row.get(3)?,
                    line: row.get(4)?,
                })
            })?;
            for row in rows {
                symbols.push(row?);
            }
        }

        let by_id: HashMap<i64, usize> = symbols
            .iter()
            .enumerate()
            .map(|(pos, s)| (s.id, pos))
            .collect();

        let mut by_name: HashMap<String, Vec<usize>> = HashMap::new();
        let eligible: Vec<bool> = symbols.iter().map(|s| is_reach_seed(&s.kind)).collect();
        for (pos, sym) in symbols.iter().enumerate() {
            by_name.entry(sym.name.clone()).or_default().push(pos);
        }

        let mut refs_by_name: HashMap<String, Vec<(usize, f64)>> = HashMap::new();
        {
            let mut stmt = conn.prepare(
                "SELECT name, caller_id, confidence FROM \"references\" \
                 WHERE caller_id IS NOT NULL",
            )?;
            let rows = stmt.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, f64>(2)?,
                ))
            })?;
            for row in rows {
                let (name, caller_id, confidence) = row?;
                if let Some(&caller_pos) = by_id.get(&caller_id) {
                    refs_by_name
                        .entry(name)
                        .or_default()
                        .push((caller_pos, confidence));
                }
            }
        }

        let mut children_by_parent_name: HashMap<String, Vec<usize>> = HashMap::new();
        {
            let mut stmt = conn.prepare(
                "SELECT p.name, c.id FROM type_edges te \
                 JOIN symbols p ON p.id = te.parent_id \
                 JOIN symbols c ON c.id = te.child_id",
            )?;
            let rows = stmt.query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            })?;
            for row in rows {
                let (parent_name, child_id) = row?;
                if let Some(&child_pos) = by_id.get(&child_id) {
                    children_by_parent_name
                        .entry(parent_name)
                        .or_default()
                        .push(child_pos);
                }
            }
        }

        Ok(Self {
            symbols,
            eligible,
            by_name,
            refs_by_name,
            children_by_parent_name,
        })
    }

    /// Candidates calling `name`: one entry per caller symbol row carrying
    /// the MAX confidence among that row's references to `name`, ordered by
    /// (file, line) — the deterministic representative rules shared with
    /// blast's ordered traversal.
    fn caller_candidates(&self, name: &str) -> Vec<(usize, f64)> {
        let Some(refs) = self.refs_by_name.get(name) else {
            return Vec::new();
        };
        let mut max_conf: HashMap<usize, f64> = HashMap::with_capacity(refs.len());
        for (caller_pos, confidence) in refs {
            let slot = max_conf.entry(*caller_pos).or_insert(*confidence);
            if *confidence > *slot {
                *slot = *confidence;
            }
        }
        let mut candidates: Vec<(usize, f64)> = max_conf.into_iter().collect();
        candidates.sort_by(|a, b| {
            let (sa, sb) = (&self.symbols[a.0], &self.symbols[b.0]);
            sa.file.cmp(&sb.file).then(sa.line.cmp(&sb.line))
        });
        candidates
    }

    /// Type-edge children of any symbol named `name`, ordered by (file, line).
    fn child_candidates(&self, name: &str) -> Vec<usize> {
        let Some(children) = self.children_by_parent_name.get(name) else {
            return Vec::new();
        };
        let mut candidates = children.clone();
        candidates.sort_by(|a, b| {
            let (sa, sb) = (&self.symbols[*a], &self.symbols[*b]);
            sa.file.cmp(&sb.file).then(sa.line.cmp(&sb.line))
        });
        candidates
    }
}

/// Build the reach table to `opts.depth` inside the caller's transaction,
/// replacing any previous contents (AR-028: rows publish atomically with
/// the caller's own writes in this transaction).
///
/// The traversal mirrors `blast::analyze_blast`'s upstream loop exactly —
/// name-keyed BFS, dedup by (name, file), expansion dedup by name,
/// type-edge children only at depth 1, eligibility via [`edge_eligible`] —
/// so the table equals the BFS result at any depth ≤ `opts.depth`.
pub fn build_reach(
    tx: &rusqlite::Transaction,
    opts: &ReachBuildOptions,
) -> Result<ReachBuildStats> {
    crate::db::ensure_reach_table(tx)?;

    let graph = ReachGraph::load(tx)?;
    let filter = EdgeFilter::default();

    // Deterministic source-name order keeps row insertion stable.
    let mut names: Vec<&String> = graph.by_name.keys().collect();
    names.sort();

    let mut rows: Vec<(i64, i64, i64, f64)> = Vec::new();
    let mut truncated_sources: Vec<i64> = Vec::new();
    let mut sources = 0usize;

    for name in names {
        let positions = &graph.by_name[name.as_str()];
        let Some(source_pos) = positions.iter().copied().find(|&p| graph.eligible[p]) else {
            continue; // Module-only names are not precomputation targets.
        };
        sources += 1;
        let source_id = graph.symbols[source_pos].id;

        let mut visited: HashSet<(String, String)> = HashSet::new();
        let mut queued: HashSet<String> = HashSet::new();
        let mut queue: VecDeque<(String, usize)> = VecDeque::new();
        queue.push_back((name.clone(), 1));
        queued.insert(name.clone());

        let mut recorded = 0usize;
        let mut truncated = false;
        // The fan-out cap halts the whole traversal; the recorded rows are a
        // deterministic BFS prefix (a lower bound on the true reach set).
        'traversal: while let Some((target_name, depth)) = queue.pop_front() {
            if depth > opts.depth {
                continue;
            }

            for (caller_pos, confidence) in graph.caller_candidates(&target_name) {
                if recorded == opts.max_targets {
                    truncated = true;
                    break 'traversal;
                }
                let sym = &graph.symbols[caller_pos];
                if !edge_eligible(&sym.file, confidence, &filter) {
                    continue;
                }
                let key = (sym.name.clone(), sym.file.clone());
                if visited.contains(&key) {
                    continue;
                }
                visited.insert(key);
                rows.push((source_id, sym.id, depth as i64, confidence));
                recorded += 1;
                if depth < opts.depth && !queued.contains(&sym.name) {
                    queued.insert(sym.name.clone());
                    queue.push_back((sym.name.clone(), depth + 1));
                }
            }

            // Type-edge children only for the initially queried name
            // (depth == 1), mirroring PRD-HRTG-REQ-003 in blast.
            if depth == 1 {
                for child_pos in graph.child_candidates(&target_name) {
                    if recorded == opts.max_targets {
                        truncated = true;
                        break 'traversal;
                    }
                    let sym = &graph.symbols[child_pos];
                    if !edge_eligible(&sym.file, 1.0, &filter) {
                        continue;
                    }
                    let key = (sym.name.clone(), sym.file.clone());
                    if visited.contains(&key) {
                        continue;
                    }
                    visited.insert(key);
                    rows.push((source_id, sym.id, depth as i64, 1.0));
                    recorded += 1;
                    if depth < opts.depth && !queued.contains(&sym.name) {
                        queued.insert(sym.name.clone());
                        queue.push_back((sym.name.clone(), depth + 1));
                    }
                }
            }
        }

        if truncated {
            truncated_sources.push(source_id);
        }
    }

    // Phase 3: replace previous contents inside the caller's transaction.
    tx.execute("DELETE FROM reach", [])?;
    tx.execute("DELETE FROM reach_truncated", [])?;
    tx.execute("DELETE FROM reach_meta", [])?;

    rows.sort_unstable_by_key(|r| (r.0, r.1));
    // 4 bound parameters per row; bundled SQLite allows 32766 variables.
    const ROWS_PER_STMT: usize = 249;
    for chunk in rows.chunks(ROWS_PER_STMT) {
        let placeholders = chunk
            .iter()
            .map(|_| "(?, ?, ?, ?)")
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "INSERT INTO reach (source_id, target_id, min_depth, confidence) VALUES {placeholders}"
        );
        let mut stmt = tx.prepare(&sql)?;
        stmt.execute(rusqlite::params_from_iter(chunk.iter().flat_map(
            |(s, t, d, c)| {
                [
                    s as &dyn rusqlite::ToSql,
                    t as &dyn rusqlite::ToSql,
                    d as &dyn rusqlite::ToSql,
                    c as &dyn rusqlite::ToSql,
                ]
            },
        )))?;
    }

    for source_id in &truncated_sources {
        tx.execute(
            "INSERT OR REPLACE INTO reach_truncated (source_id) VALUES (?1)",
            rusqlite::params![source_id],
        )?;
    }

    tx.execute(
        "INSERT INTO reach_meta (key, value) VALUES (?1, ?2)",
        rusqlite::params![META_BUILT_DEPTH, opts.depth.to_string()],
    )?;

    Ok(ReachBuildStats {
        sources,
        rows: rows.len(),
        truncated_sources: truncated_sources.len(),
    })
}

/// An authoritative answer from the reach table. `Some(ReachAnswer)` means
/// the table covers the query; `Some` with an empty `affected` vec means the
/// covered symbol genuinely has no dependants (never silence).
#[derive(Debug, Clone, PartialEq)]
pub struct ReachAnswer {
    pub affected: Vec<BlastAffectedSymbol>,
    pub truncated: bool,
}

/// Answer an upstream blast query from the precomputed table.
///
/// Returns `None` when the table cannot authoritatively answer — absent or
/// stale table, query deeper than `built_depth`, or a name with no eligible
/// (non-Module) symbol — in which case the caller must run the live BFS.
/// All reads run inside one deferred read transaction so the meta check,
/// id resolution, row fetch, and truncation fetch observe a single snapshot
/// (AR-028).
pub fn lookup_upstream(
    conn: &Connection,
    symbol: &str,
    depth: usize,
) -> Result<Option<ReachAnswer>> {
    let tx = conn
        .unchecked_transaction()
        .context("starting reach read")?;
    let answer = lookup_upstream_impl(&tx, symbol, depth);
    // A read transaction's commit is a no-op release of the snapshot.
    tx.commit().context("finishing reach read")?;
    answer
}

/// Core lookup without transaction management, so callers that already hold
/// a read transaction (concurrency tests, TASK-081) can share one snapshot.
pub(crate) fn lookup_upstream_impl(
    conn: &Connection,
    symbol: &str,
    depth: usize,
) -> Result<Option<ReachAnswer>> {
    // Pre-V5 index: no reach tables at all — nothing to answer from.
    let table_exists: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='reach'",
        [],
        |row| row.get(0),
    )?;
    if table_exists == 0 {
        return Ok(None);
    }

    let built: Option<String> = conn
        .query_row(
            "SELECT value FROM reach_meta WHERE key = ?1",
            rusqlite::params![META_BUILT_DEPTH],
            |row| row.get(0),
        )
        .ok();
    let Some(built) = built.and_then(|v| v.parse::<usize>().ok()) else {
        return Ok(None);
    };
    if built < depth {
        return Ok(None);
    }
    let stale: i64 = conn.query_row(
        "SELECT COUNT(*) FROM reach_meta WHERE key = ?1",
        rusqlite::params![META_STALE],
        |row| row.get(0),
    )?;
    if stale > 0 {
        return Ok(None);
    }

    // Resolve the queried name to its canonical source id: MIN(id) over the
    // name's eligible symbols (the same rule the build wrote rows under).
    let mut stmt = conn.prepare("SELECT id, kind FROM symbols WHERE name = ?1")?;
    let ids = stmt
        .query_map(rusqlite::params![symbol], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let source_id = ids
        .iter()
        .filter(|(_, kind)| {
            SymbolKind::from_str(kind)
                .map(|k| is_reach_seed(&k))
                .unwrap_or(true)
        })
        .map(|(id, _)| *id)
        .min();
    let Some(source_id) = source_id else {
        return Ok(None);
    };

    let mut stmt = conn.prepare(
        "SELECT s.name, s.kind, s.file, s.line, r.min_depth, r.confidence \
         FROM reach r JOIN symbols s ON s.id = r.target_id \
         WHERE r.source_id = ?1 AND r.min_depth <= ?2",
    )?;
    let affected: Vec<BlastAffectedSymbol> = stmt
        .query_map(rusqlite::params![source_id, depth as i64], |row| {
            Ok(BlastAffectedSymbol {
                name: row.get(0)?,
                kind: SymbolKind::from_str(&row.get::<_, String>(1)?)
                    .unwrap_or(SymbolKind::Function),
                file: row.get(2)?,
                line: row.get::<_, i64>(3)? as usize,
                depth: row.get::<_, i64>(4)? as usize,
                confidence: row.get(5)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;

    let truncated: i64 = conn.query_row(
        "SELECT COUNT(*) FROM reach_truncated WHERE source_id = ?1",
        rusqlite::params![source_id],
        |row| row.get(0),
    )?;

    Ok(Some(ReachAnswer {
        affected,
        truncated: truncated > 0,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;
    use tempfile::TempDir;

    #[test]
    fn is_reach_seed_truth_table() {
        // Module is the only excluded kind (structural, not a change target).
        assert!(!is_reach_seed(&SymbolKind::Module));

        // Every other kind is an eligible change target.
        for kind in [
            SymbolKind::Function,
            SymbolKind::Method,
            SymbolKind::Class,
            SymbolKind::Struct,
            SymbolKind::Interface,
            SymbolKind::Enum,
            SymbolKind::Trait,
            SymbolKind::TypeAlias,
            SymbolKind::Constant,
            SymbolKind::Variable,
        ] {
            assert!(
                is_reach_seed(&kind),
                "{kind} should be an eligible reach seed"
            );
        }
    }

    #[test]
    fn edge_eligible_truth_table() {
        let default = EdgeFilter::default();

        // Plain production edge: eligible.
        assert!(edge_eligible("src/lib.rs", 0.5, &default));

        // Test-file discovery is excluded by default...
        assert!(!edge_eligible("tests/it.rs", 0.95, &default));
        // ...unless tests are included.
        let with_tests = EdgeFilter {
            include_tests: true,
            ..default.clone()
        };
        assert!(edge_eligible("tests/it.rs", 0.95, &with_tests));

        // Confidence floor: edges below it are excluded.
        let strict = EdgeFilter {
            min_confidence: 0.9,
            ..default.clone()
        };
        assert!(!edge_eligible("src/lib.rs", 0.5, &strict));
        assert!(edge_eligible("src/lib.rs", 0.95, &strict));
        // Floor is inclusive: exactly-at-threshold passes.
        assert!(edge_eligible("src/lib.rs", 0.9, &strict));

        // A test file still fails even when confidence passes.
        assert!(!edge_eligible("tests/it.rs", 1.0, &strict));
    }

    // -- Build tests ---------------------------------------------------------

    /// Test DB with the full schema (reach tables included) on a TempDir.
    fn make_db() -> (TempDir, Connection) {
        let dir = TempDir::new().unwrap();
        let conn = db::open(&dir.path().join("index.db")).unwrap();
        (dir, conn)
    }

    fn insert_symbol(conn: &Connection, name: &str, kind: &str, file: &str, line: i64) -> i64 {
        conn.execute(
            "INSERT INTO symbols (name, kind, file, line, col, language) \
             VALUES (?1, ?2, ?3, ?4, 1, 'rust')",
            rusqlite::params![name, kind, file, line],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    fn insert_ref(conn: &Connection, callee: &str, caller_id: Option<i64>, confidence: f64) {
        conn.execute(
            "INSERT INTO \"references\" (name, file, line, col, caller_id, confidence) \
             VALUES (?1, 'src/lib.rs', 1, 1, ?2, ?3)",
            rusqlite::params![callee, caller_id, confidence],
        )
        .unwrap();
    }

    fn insert_type_edge(conn: &Connection, parent_id: i64, child_id: i64) {
        conn.execute(
            "INSERT INTO type_edges (child_id, parent_id, relationship) VALUES (?1, ?2, 'impl')",
            rusqlite::params![child_id, parent_id],
        )
        .unwrap();
    }

    fn build(conn: &Connection, depth: usize, max_targets: usize) -> ReachBuildStats {
        let tx = conn.unchecked_transaction().unwrap();
        let stats = build_reach(&tx, &ReachBuildOptions { depth, max_targets }).unwrap();
        tx.commit().unwrap();
        stats
    }

    /// (target_id, min_depth, confidence) rows for one source, ordered.
    fn reach_rows(conn: &Connection, source_id: i64) -> Vec<(i64, i64, f64)> {
        conn.prepare("SELECT target_id, min_depth, confidence FROM reach WHERE source_id = ?1 ORDER BY target_id")
            .unwrap()
            .query_map(rusqlite::params![source_id], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    }

    fn built_depth(conn: &Connection) -> Option<String> {
        conn.query_row(
            "SELECT value FROM reach_meta WHERE key = 'built_depth'",
            [],
            |row| row.get(0),
        )
        .ok()
    }

    fn is_stale(conn: &Connection) -> bool {
        conn.query_row(
            "SELECT COUNT(*) FROM reach_meta WHERE key = 'stale'",
            [],
            |row| row.get::<_, i64>(0),
        )
        .unwrap()
            > 0
    }

    #[test]
    fn build_chain_records_min_depths() {
        let (_dir, conn) = make_db();
        let a = insert_symbol(&conn, "a", "function", "src/a.rs", 1);
        let b = insert_symbol(&conn, "b", "function", "src/a.rs", 10);
        let c = insert_symbol(&conn, "c", "function", "src/a.rs", 20);
        let d = insert_symbol(&conn, "d", "function", "src/a.rs", 30);
        insert_ref(&conn, "b", Some(a), 0.9);
        insert_ref(&conn, "c", Some(b), 0.9);
        insert_ref(&conn, "d", Some(c), 0.9);

        let stats = build(&conn, 3, DEFAULT_MAX_TARGETS_PER_SOURCE);

        assert_eq!(
            reach_rows(&conn, d),
            vec![(a, 3, 0.9), (b, 2, 0.9), (c, 1, 0.9)]
        );
        // From c: b is a direct caller, a is two hops out.
        assert_eq!(reach_rows(&conn, c), vec![(a, 2, 0.9), (b, 1, 0.9)]);
        // From b: a only. From a: nothing.
        assert_eq!(stats.rows, 6);
    }

    #[test]
    fn build_diamond_min_depth_wins() {
        let (_dir, conn) = make_db();
        let a = insert_symbol(&conn, "a", "function", "src/a.rs", 1);
        let b = insert_symbol(&conn, "b", "function", "src/b.rs", 1);
        let c = insert_symbol(&conn, "c", "function", "src/c.rs", 1);
        let d = insert_symbol(&conn, "d", "function", "src/d.rs", 1);
        // a -> b -> d and a -> c -> d: d reaches a at depth 2 via both paths.
        insert_ref(&conn, "b", Some(a), 0.8);
        insert_ref(&conn, "c", Some(a), 0.9);
        insert_ref(&conn, "d", Some(b), 0.7);
        insert_ref(&conn, "d", Some(c), 0.6);

        build(&conn, 3, DEFAULT_MAX_TARGETS_PER_SOURCE);

        // Both depth-1 callers recorded; a recorded exactly once at depth 2
        // (first discovery wins, FIFO BFS).
        assert_eq!(
            reach_rows(&conn, d),
            vec![(a, 2, 0.8), (b, 1, 0.7), (c, 1, 0.6)]
        );
    }

    #[test]
    fn build_cycle_terminates_and_records_self_row() {
        let (_dir, conn) = make_db();
        // Mutual recursion a -> b -> a.
        let a = insert_symbol(&conn, "a", "function", "src/a.rs", 1);
        let b = insert_symbol(&conn, "b", "function", "src/b.rs", 1);
        insert_ref(&conn, "b", Some(a), 0.9);
        insert_ref(&conn, "a", Some(b), 0.9);

        build(&conn, 3, DEFAULT_MAX_TARGETS_PER_SOURCE);

        // From a: b is the depth-1 caller; a itself is re-discovered at
        // depth 2 through the cycle (legitimate self-row, no hang).
        assert_eq!(reach_rows(&conn, a), vec![(a, 2, 0.9), (b, 1, 0.9)]);
    }

    #[test]
    fn build_direct_self_call_records_depth_1() {
        let (_dir, conn) = make_db();
        let a = insert_symbol(&conn, "a", "function", "src/a.rs", 1);
        insert_ref(&conn, "a", Some(a), 0.9);

        build(&conn, 3, DEFAULT_MAX_TARGETS_PER_SOURCE);

        assert_eq!(reach_rows(&conn, a), vec![(a, 1, 0.9)]);
    }

    #[test]
    fn build_excludes_test_file_callers() {
        let (_dir, conn) = make_db();
        let prod = insert_symbol(&conn, "prod", "function", "src/prod.rs", 1);
        let test_caller = insert_symbol(&conn, "test_caller", "function", "tests/it.rs", 1);
        insert_ref(&conn, "target", Some(prod), 0.9);
        insert_ref(&conn, "target", Some(test_caller), 0.9);
        insert_symbol(&conn, "target", "function", "src/prod.rs", 50);

        build(&conn, 3, DEFAULT_MAX_TARGETS_PER_SOURCE);

        assert_eq!(
            reach_rows(&conn, 3),
            vec![(prod, 1, 0.9)],
            "test-file caller must not be recorded"
        );
    }

    #[test]
    fn build_ignores_null_caller_id_references() {
        let (_dir, conn) = make_db();
        let target = insert_symbol(&conn, "target", "function", "src/a.rs", 1);
        insert_ref(&conn, "target", None, 0.99);

        build(&conn, 3, DEFAULT_MAX_TARGETS_PER_SOURCE);

        assert!(
            reach_rows(&conn, target).is_empty(),
            "unresolved caller references contribute nothing"
        );
    }

    #[test]
    fn build_records_module_caller_as_target() {
        let (_dir, conn) = make_db();
        let module = insert_symbol(&conn, "mod_impl", "module", "src/a.rs", 1);
        let target = insert_symbol(&conn, "target", "function", "src/a.rs", 30);
        insert_ref(&conn, "target", Some(module), 0.9);

        build(&conn, 3, DEFAULT_MAX_TARGETS_PER_SOURCE);

        // Module is excluded as a *seed* but recorded as a *target*,
        // matching blast's behavior for module-shaped callers.
        assert_eq!(reach_rows(&conn, target), vec![(module, 1, 0.9)]);
    }

    #[test]
    fn build_module_only_name_has_no_rows() {
        let (_dir, conn) = make_db();
        let module = insert_symbol(&conn, "only_mod", "module", "src/a.rs", 1);
        insert_ref(&conn, "only_mod", Some(module), 0.9);

        let stats = build(&conn, 3, DEFAULT_MAX_TARGETS_PER_SOURCE);

        assert!(reach_rows(&conn, module).is_empty());
        assert_eq!(stats.sources, 0, "module-only names are not sources");
    }

    #[test]
    fn build_uses_min_eligible_id_for_colliding_names() {
        let (_dir, conn) = make_db();
        // Name "Foo" exists as a Module (id 1, lowest) and a Struct (id 2).
        // The canonical source id is MIN over ELIGIBLE symbols, so 2.
        let module = insert_symbol(&conn, "Foo", "module", "src/a.rs", 1);
        let structure = insert_symbol(&conn, "Foo", "struct", "src/b.rs", 1);
        let caller = insert_symbol(&conn, "caller", "function", "src/c.rs", 1);
        insert_ref(&conn, "Foo", Some(caller), 0.9);

        let stats = build(&conn, 3, DEFAULT_MAX_TARGETS_PER_SOURCE);

        assert!(reach_rows(&conn, module).is_empty(), "module id not used");
        assert_eq!(reach_rows(&conn, structure), vec![(caller, 1, 0.9)]);
        assert_eq!(stats.sources, 2, "Foo and caller are the two source names");
    }

    #[test]
    fn build_records_deterministic_representative() {
        let (_dir, conn) = make_db();
        // Two callers named "dup" in one file: line 10 (refs 0.5 + 0.8) and
        // line 20 (ref 0.9). The min-line row wins with its MAX confidence.
        let dup_low = insert_symbol(&conn, "dup", "function", "src/a.rs", 10);
        let dup_high = insert_symbol(&conn, "dup", "function", "src/a.rs", 20);
        let target = insert_symbol(&conn, "target", "function", "src/a.rs", 1);
        insert_ref(&conn, "target", Some(dup_low), 0.5);
        insert_ref(&conn, "target", Some(dup_low), 0.8);
        insert_ref(&conn, "target", Some(dup_high), 0.9);

        build(&conn, 3, DEFAULT_MAX_TARGETS_PER_SOURCE);

        assert_eq!(
            reach_rows(&conn, target),
            vec![(dup_low, 1, 0.8)],
            "min-line representative, max confidence among its refs"
        );
    }

    #[test]
    fn build_records_type_edge_children_at_depth_1_only() {
        let (_dir, conn) = make_db();
        let parent = insert_symbol(&conn, "Widget", "struct", "src/a.rs", 1);
        let child = insert_symbol(&conn, "Button", "struct", "src/b.rs", 1);
        let grandchild = insert_symbol(&conn, "TinyButton", "struct", "src/c.rs", 1);
        insert_type_edge(&conn, parent, child);
        insert_type_edge(&conn, child, grandchild);

        build(&conn, 3, DEFAULT_MAX_TARGETS_PER_SOURCE);

        // Children enter only at depth 1 from the queried name: Button is a
        // child of Widget; TinyButton is a child of Button (not of Widget).
        assert_eq!(reach_rows(&conn, parent), vec![(child, 1, 1.0)]);
        assert_eq!(reach_rows(&conn, child), vec![(grandchild, 1, 1.0)]);
    }

    #[test]
    fn build_type_edge_children_by_parent_name_union() {
        let (_dir, conn) = make_db();
        // Two symbols named Impl (impl blocks) both parent the same child.
        let impl_a = insert_symbol(&conn, "Impl", "module", "src/a.rs", 1);
        let impl_b = insert_symbol(&conn, "Impl", "module", "src/b.rs", 1);
        let child = insert_symbol(&conn, "method", "method", "src/a.rs", 5);
        insert_type_edge(&conn, impl_a, child);
        insert_type_edge(&conn, impl_b, child);
        // And an eligible symbol named Impl so the name is a source.
        let impl_struct = insert_symbol(&conn, "Impl", "struct", "src/c.rs", 1);

        build(&conn, 3, DEFAULT_MAX_TARGETS_PER_SOURCE);

        // Children are looked up by parent NAME (union over both Impl rows):
        // the child is recorded once even though two parent rows reach it.
        let rows = reach_rows(&conn, impl_struct);
        assert_eq!(
            rows,
            vec![(child, 1, 1.0)],
            "child found via either parent row with the same name"
        );
    }

    #[test]
    fn build_cap_truncates_and_marks() {
        let (_dir, conn) = make_db();
        let target = insert_symbol(&conn, "hub", "function", "src/a.rs", 1);
        let mut caller_ids = Vec::new();
        for i in 0..3 {
            caller_ids.push(insert_symbol(
                &conn,
                &format!("c{i}"),
                "function",
                &format!("src/f{i}.rs"),
                1,
            ));
            insert_ref(&conn, "hub", Some(caller_ids[i]), 0.9);
        }

        let stats = build(&conn, 3, 2);

        let rows = reach_rows(&conn, target);
        assert_eq!(rows.len(), 2, "cap bounds the recorded set");
        assert_eq!(stats.truncated_sources, 1);
        let marked: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM reach_truncated WHERE source_id = ?1",
                rusqlite::params![target],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(marked, 1, "truncated source is marked in reach_truncated");

        // Capped rows are a prefix (subset) of the uncapped set.
        conn.execute("DELETE FROM reach", []).unwrap();
        conn.execute("DELETE FROM reach_truncated", []).unwrap();
        conn.execute("DELETE FROM reach_meta", []).unwrap();
        build(&conn, 3, DEFAULT_MAX_TARGETS_PER_SOURCE);
        let uncapped = reach_rows(&conn, target);
        assert!(rows.iter().all(|r| uncapped.contains(r)));
        assert_eq!(uncapped.len(), 3);
    }

    #[test]
    fn build_exactly_at_cap_without_overflow_not_marked() {
        let (_dir, conn) = make_db();
        let target = insert_symbol(&conn, "hub", "function", "src/a.rs", 1);
        let c0 = insert_symbol(&conn, "c0", "function", "src/f0.rs", 1);
        let c1 = insert_symbol(&conn, "c1", "function", "src/f1.rs", 1);
        insert_ref(&conn, "hub", Some(c0), 0.9);
        insert_ref(&conn, "hub", Some(c1), 0.9);

        let stats = build(&conn, 3, 2);

        assert_eq!(
            stats.truncated_sources, 0,
            "natural fit at cap is not truncation"
        );
        assert!(reach_rows(&conn, target).len() == 2);
    }

    #[test]
    fn build_writes_meta_and_replaces_previous() {
        let (_dir, conn) = make_db();
        let a = insert_symbol(&conn, "a", "function", "src/a.rs", 1);
        let b = insert_symbol(&conn, "b", "function", "src/a.rs", 10);
        insert_ref(&conn, "b", Some(a), 0.9);

        build(&conn, 3, DEFAULT_MAX_TARGETS_PER_SOURCE);
        assert_eq!(built_depth(&conn).as_deref(), Some("3"));
        assert!(!is_stale(&conn));
        assert_eq!(reach_rows(&conn, b), vec![(a, 1, 0.9)]);

        // Graph changes; a rebuild must fully replace prior rows.
        let c = insert_symbol(&conn, "c", "function", "src/a.rs", 20);
        insert_ref(&conn, "b", Some(c), 0.9);
        conn.execute(
            "DELETE FROM \"references\" WHERE caller_id = ?1",
            rusqlite::params![a],
        )
        .unwrap();

        build(&conn, 3, DEFAULT_MAX_TARGETS_PER_SOURCE);
        assert_eq!(
            reach_rows(&conn, b),
            vec![(c, 1, 0.9)],
            "stale row for a must be gone after rebuild"
        );
        assert_eq!(built_depth(&conn).as_deref(), Some("3"));
    }

    #[test]
    fn build_depth_one_records_only_direct_edges() {
        let (_dir, conn) = make_db();
        let a = insert_symbol(&conn, "a", "function", "src/a.rs", 1);
        let b = insert_symbol(&conn, "b", "function", "src/a.rs", 10);
        let c = insert_symbol(&conn, "c", "function", "src/a.rs", 20);
        insert_ref(&conn, "b", Some(a), 0.9);
        insert_ref(&conn, "c", Some(b), 0.9);

        let stats = build(&conn, 1, DEFAULT_MAX_TARGETS_PER_SOURCE);

        assert_eq!(reach_rows(&conn, c), vec![(b, 1, 0.9)]);
        assert_eq!(built_depth(&conn).as_deref(), Some("1"));
        assert!(stats.rows >= 1);
    }

    #[test]
    fn build_on_missing_tables_is_safe() {
        // A pre-V5 database lacking the reach tables must not panic — the
        // build ensures them before writing.
        let dir = TempDir::new().unwrap();
        let conn = Connection::open(dir.path().join("old.db")).unwrap();
        conn.execute_batch(
            "CREATE TABLE symbols (id INTEGER PRIMARY KEY, name TEXT NOT NULL, kind TEXT NOT NULL, \
             file TEXT NOT NULL, line INTEGER NOT NULL, col INTEGER NOT NULL, language TEXT NOT NULL); \
             CREATE TABLE \"references\" (id INTEGER PRIMARY KEY, name TEXT NOT NULL, file TEXT NOT NULL, \
             line INTEGER NOT NULL, col INTEGER NOT NULL, caller_id INTEGER, confidence REAL); \
             CREATE TABLE type_edges (id INTEGER PRIMARY KEY, child_id INTEGER NOT NULL, \
             parent_id INTEGER NOT NULL, relationship TEXT NOT NULL);",
        )
        .unwrap();
        let a = insert_symbol(&conn, "a", "function", "src/a.rs", 1);
        insert_ref(&conn, "b", Some(a), 0.9);
        insert_symbol(&conn, "b", "function", "src/a.rs", 5);

        let stats = build(&conn, 3, DEFAULT_MAX_TARGETS_PER_SOURCE);

        assert_eq!(stats.sources, 2);
        assert_eq!(stats.rows, 1);
    }

    // -- Lookup tests --------------------------------------------------------

    /// Chain a -> b -> c -> d plus an isolated leaf with no callers.
    fn chain_db() -> (TempDir, Connection) {
        let (dir, conn) = make_db();
        let a = insert_symbol(&conn, "a", "function", "src/a.rs", 1);
        let b = insert_symbol(&conn, "b", "function", "src/a.rs", 10);
        let c = insert_symbol(&conn, "c", "method", "src/a.rs", 20);
        insert_symbol(&conn, "d", "function", "src/a.rs", 30);
        insert_ref(&conn, "b", Some(a), 0.8);
        insert_ref(&conn, "c", Some(b), 0.85);
        insert_ref(&conn, "d", Some(c), 0.95);
        // Unknown kind exercises the from_str fallback on lookup.
        let weird = insert_symbol(&conn, "weird", "bogus_kind", "src/w.rs", 1);
        insert_ref(&conn, "d", Some(weird), 0.7);
        let _leaf = insert_symbol(&conn, "leaf", "function", "src/l.rs", 1);
        build(&conn, 3, DEFAULT_MAX_TARGETS_PER_SOURCE);
        (dir, conn)
    }

    #[test]
    fn lookup_covered_name_returns_affected_set() {
        let (_dir, conn) = chain_db();

        let answer = lookup_upstream(&conn, "d", 3).unwrap().expect("covered");

        assert!(!answer.truncated);
        let by_name: HashMap<&str, &BlastAffectedSymbol> = answer
            .affected
            .iter()
            .map(|s| (s.name.as_str(), s))
            .collect();
        assert_eq!(by_name.len(), 4);
        assert_eq!(by_name["c"].kind, SymbolKind::Method);
        assert_eq!(by_name["c"].file, "src/a.rs");
        assert_eq!(by_name["c"].depth, 1);
        assert_eq!(by_name["c"].confidence, 0.95);
        assert_eq!(by_name["b"].depth, 2);
        assert_eq!(by_name["a"].depth, 3);
        // Unknown kind falls back to Function, mirroring blast.
        assert_eq!(by_name["weird"].kind, SymbolKind::Function);
        assert_eq!(by_name["weird"].depth, 1);
    }

    #[test]
    fn lookup_depth_filters_min_depth() {
        let (_dir, conn) = chain_db();

        let answer = lookup_upstream(&conn, "d", 1).unwrap().expect("covered");
        assert!(answer.affected.iter().all(|s| s.depth <= 1));
        assert_eq!(answer.affected.len(), 2, "only depth-1 rows");

        let answer = lookup_upstream(&conn, "d", 2).unwrap().expect("covered");
        assert_eq!(answer.affected.len(), 3, "depth-1 and depth-2 rows");
    }

    #[test]
    fn lookup_beyond_built_depth_is_none() {
        let (_dir, conn) = chain_db();
        assert!(
            lookup_upstream(&conn, "d", 4).unwrap().is_none(),
            "beyond built_depth must fall back to BFS (REQ-004)"
        );
    }

    #[test]
    fn lookup_stale_table_is_none() {
        let (_dir, conn) = chain_db();
        let tx = conn.unchecked_transaction().unwrap();
        mark_stale(&tx).unwrap();
        tx.commit().unwrap();

        assert!(
            lookup_upstream(&conn, "d", 3).unwrap().is_none(),
            "stale table must fall back to BFS (REQ-007)"
        );
    }

    #[test]
    fn lookup_not_built_table_is_none() {
        let (_dir, conn) = make_db();
        let a = insert_symbol(&conn, "a", "function", "src/a.rs", 1);
        insert_symbol(&conn, "b", "function", "src/a.rs", 5);
        insert_ref(&conn, "b", Some(a), 0.9);
        // No build has run.

        assert!(
            lookup_upstream(&conn, "b", 3).unwrap().is_none(),
            "absent table must fall back to BFS without erroring"
        );
    }

    #[test]
    fn lookup_pre_v5_database_without_reach_tables_is_none() {
        let dir = TempDir::new().unwrap();
        let conn = Connection::open(dir.path().join("v4.db")).unwrap();
        conn.execute_batch("CREATE TABLE symbols (id INTEGER PRIMARY KEY, name TEXT);")
            .unwrap();
        conn.execute("INSERT INTO symbols (name) VALUES ('x')", [])
            .unwrap();

        // A reader on an old index: no reach tables, no error, just None.
        assert!(lookup_upstream(&conn, "x", 3).unwrap().is_none());
    }

    #[test]
    fn lookup_no_eligible_symbol_is_none() {
        let (_dir, conn) = chain_db();
        // Unknown name and module-only names cannot be precomputed sources.
        assert!(lookup_upstream(&conn, "missing", 3).unwrap().is_none());
        conn.execute(
            "INSERT INTO symbols (name, kind, file, line, col, language) \
             VALUES ('only_mod', 'module', 'src/m.rs', 1, 1, 'rust')",
            [],
        )
        .unwrap();
        assert!(lookup_upstream(&conn, "only_mod", 3).unwrap().is_none());
    }

    #[test]
    fn lookup_covered_with_zero_dependents_is_some_empty() {
        let (_dir, conn) = chain_db();

        let answer = lookup_upstream(&conn, "leaf", 3)
            .unwrap()
            .expect("authoritative empty");
        assert!(answer.affected.is_empty());
        assert!(!answer.truncated);
    }

    #[test]
    fn lookup_reports_truncation_marker() {
        let (_dir, conn) = make_db();
        let hub = insert_symbol(&conn, "hub", "function", "src/a.rs", 1);
        for i in 0..3 {
            let c = insert_symbol(
                &conn,
                &format!("c{i}"),
                "function",
                &format!("src/f{i}.rs"),
                1,
            );
            insert_ref(&conn, "hub", Some(c), 0.9);
        }
        build(&conn, 3, 2);

        let answer = lookup_upstream(&conn, "hub", 3).unwrap().expect("covered");
        assert_eq!(answer.affected.len(), 2);
        assert!(
            answer.truncated,
            "truncation must be identifiable from the result alone (REQ-009)"
        );
        let _ = hub;
    }

    #[test]
    fn lookup_uses_index_not_table_scan() {
        let (_dir, conn) = chain_db();

        let plan: String = conn
            .prepare(
                "EXPLAIN QUERY PLAN \
                 SELECT s.name, s.kind, s.file, s.line, r.min_depth, r.confidence \
                 FROM reach r JOIN symbols s ON s.id = r.target_id \
                 WHERE r.source_id = 1 AND r.min_depth <= 3",
            )
            .unwrap()
            .query_map([], |row| row.get::<_, String>(3))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
            .join(" | ");

        assert!(
            plan.contains("idx_reach_source_depth"),
            "lookup must use the covering index, plan was: {plan}"
        );
        assert!(!plan.contains("SCAN reach"), "no full scans: {plan}");
    }
}
