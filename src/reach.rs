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

/// The shared name-keyed BFS traversal core (AR-021).
///
/// One state holder, two engines: `blast::analyze_blast`'s live BFS and
/// [`build_reach`]'s table build both run their traversal through it, so the
/// rule the table's equivalence rests on — record each `(name, file)` once,
/// expand each name once, stop expanding at `max_depth` — exists as this one
/// copy instead of two discipline-synced ones. Candidate enumeration stays
/// per-engine (SQL in blast, [`ReachGraph`] in reach); only the
/// visited/queued/enqueue mechanics are shared.
pub(crate) struct NameBfs {
    /// `(name, file)` pairs already recorded — prevents output duplicates.
    visited: HashSet<(String, String)>,
    /// Symbol names already enqueued — prevents BFS re-expansion.
    queued: HashSet<String>,
    /// FIFO frontier of `(name, depth)`.
    queue: VecDeque<(String, usize)>,
}

impl NameBfs {
    /// Start a traversal at `root` (depth 1), marking it queued.
    pub(crate) fn new(root: &str) -> Self {
        let mut bfs = Self {
            visited: HashSet::new(),
            queued: HashSet::new(),
            queue: VecDeque::new(),
        };
        bfs.queued.insert(root.to_string());
        bfs.queue.push_back((root.to_string(), 1));
        bfs
    }

    /// Pop the next frontier entry, FIFO.
    pub(crate) fn pop(&mut self) -> Option<(String, usize)> {
        self.queue.pop_front()
    }

    /// Try to record a discovered symbol and expand its name.
    ///
    /// Insert-if-new on `(name, file)`: returns `false` (no state change)
    /// when the pair was already recorded. On the first discovery, enqueues
    /// `(name, depth + 1)` iff `depth < max_depth` and the name was not
    /// already queued, and returns `true` — the caller records its row.
    pub(crate) fn admit(&mut self, name: &str, file: &str, depth: usize, max_depth: usize) -> bool {
        if !self.visited.insert((name.to_string(), file.to_string())) {
            return false;
        }
        if depth < max_depth && !self.queued.contains(name) {
            self.queued.insert(name.to_string());
            self.queue.push_back((name.to_string(), depth + 1));
        }
        true
    }
}

/// A traversal candidate: a symbol row the BFS may record as a target.
///
/// Both engines enumerate candidates as these values — the full build from
/// the in-memory [`ReachGraph`] ([`GraphCandidates`]) and the incremental
/// repair from indexed SQL ([`SqlCandidates`]) — so their candidate lists
/// can be compared for exact equality, which is the
/// equivalence-by-construction guarantee the repair rests on.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ReachCandidate {
    pub(crate) id: i64,
    pub(crate) name: String,
    pub(crate) kind: SymbolKind,
    pub(crate) file: String,
    pub(crate) line: i64,
    /// Confidence of the discovering edge; MAX over the caller's refs to
    /// this name for caller candidates, 1.0 for type-edge children.
    pub(crate) confidence: f64,
}

/// Candidate enumeration for one BFS step, abstracted over the two engines.
///
/// `caller_candidates(name)` lists the symbols calling `name` (one entry per
/// caller symbol row, carrying that caller's MAX confidence among its refs
/// to the name); `child_candidates(name)` lists the type-edge children of
/// any symbol named `name`. Both are ordered by (file, line, id) — the
/// deterministic representative rules shared with blast's traversal.
pub(crate) trait CandidateSource {
    fn caller_candidates(&mut self, name: &str) -> Result<&[ReachCandidate]>;
    fn child_candidates(&mut self, name: &str) -> Result<&[ReachCandidate]>;
}

/// Rows computed for one source name: `(target_id, min_depth, confidence)`.
/// The source id is the caller's choice (canonical MIN-eligible id).
pub(crate) type ComputedSourceRows = Vec<(i64, i64, f64)>;

/// One recording step of the source traversal shared by build and repair.
///
/// Runs the same name-keyed BFS loop `build_reach` always ran — cap check
/// first (a `false` return halts the whole traversal), then edge
/// eligibility, then the shared [`NameBfs`] visited/enqueue rule, then the
/// row write — over any [`CandidateSource`], so the full build (in-memory
/// graph) and the incremental repair (indexed SQL) execute one code path.
///
/// Returns `(rows, truncated)`; `truncated` is set when the fan-out cap
/// halted the traversal, making `rows` a deterministic BFS prefix.
pub(crate) fn compute_source_rows(
    src: &mut dyn CandidateSource,
    name: &str,
    opts: &ReachBuildOptions,
) -> Result<(ComputedSourceRows, bool)> {
    let filter = EdgeFilter::default();
    let mut bfs = NameBfs::new(name);
    let mut rows: ComputedSourceRows = Vec::new();
    let mut recorded = 0usize;
    let mut truncated = false;

    // Fan-out cap first: `false` means the cap was hit and the whole
    // traversal must halt; the recorded rows are a deterministic BFS prefix.
    let mut record = |bfs: &mut NameBfs, cand: &ReachCandidate, depth: usize| -> bool {
        if recorded == opts.max_targets {
            truncated = true;
            return false;
        }
        if !edge_eligible(&cand.file, cand.confidence, &filter) {
            return true;
        }
        if bfs.admit(&cand.name, &cand.file, depth, opts.depth) {
            rows.push((cand.id, depth as i64, cand.confidence));
            recorded += 1;
        }
        true
    };

    'traversal: while let Some((target_name, depth)) = bfs.pop() {
        if depth > opts.depth {
            continue;
        }

        for cand in src.caller_candidates(&target_name)? {
            if !record(&mut bfs, cand, depth) {
                break 'traversal;
            }
        }

        // Type-edge children only for the initially queried name
        // (depth == 1), mirroring PRD-HRTG-REQ-003 in blast.
        if depth == 1 {
            for cand in src.child_candidates(&target_name)? {
                if !record(&mut bfs, cand, depth) {
                    break 'traversal;
                }
            }
        }
    }

    Ok((rows, truncated))
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

// ---------------------------------------------------------------------------
// Incremental maintenance (TASK-081, PRD-REACH-REQ-005/007)
// ---------------------------------------------------------------------------

/// Bound parameters per IN-list statement. Bundled SQLite allows 32766;
/// 900 keeps every statement well under any build's limit.
const IN_CHUNK: usize = 900;

/// Pre-edit state of one file edit: [`begin_file_edit`] captures it before
/// the file's old rows are deleted, [`finish_file_edit`] consumes it after
/// the new rows are written — both inside the caller's transaction, so the
/// two halves observe one consistent timeline.
#[derive(Debug, Clone)]
pub(crate) struct FileEditScope {
    rel_path: String,
    /// Names whose candidate lists or symbol sets the file's OLD rows
    /// participated in: the file's symbol names, the callee names of its
    /// references with a resolved caller, and the parent names of type
    /// edges whose child lives in the file.
    a_pre: Vec<String>,
    /// Canonical (MIN-eligible) source ids of `a_pre` names as they stood
    /// BEFORE the edit. Capturing them pre-delete is what re-keys rows when
    /// the edit removes or demotes a name's MIN-id symbol — those rows die
    /// here and are rewritten under the post-edit canonical id.
    old_canonical_ids: Vec<i64>,
    /// Sources holding pre-edit rows that target a symbol named in `a_pre`
    /// — the reverse `target_id` lookup REQ-005 prescribes, served by
    /// `idx_reach_target`. No depth filter: the over-approximation is safe
    /// because repair is delete-then-rebuild. Subsumes the file's old
    /// symbol ids (every one of them belongs to a symbol named in `a_pre`).
    predecessor_ids: Vec<i64>,
    /// `finish` no-ops when the table is absent, never built, or stale —
    /// stale tables are only cleared by a full rebuild (REQ-007).
    skipped: bool,
}

/// Statistics from one incremental repair.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ReachRepairStats {
    /// Source names recomputed from the post-edit graph.
    pub(crate) rebuilt_sources: usize,
    /// Reach rows removed (the affected ids' whole row sets).
    pub(crate) rows_deleted: usize,
    /// Reach rows written.
    pub(crate) rows_written: usize,
    /// True when the repair was skipped: absent, never-built, or stale
    /// table.
    pub(crate) skipped: bool,
}

/// One-shot failure injection point for the REQ-007 degrade test: when set
/// to a relative path, the next `finish_file_edit` for exactly that path
/// returns an error and clears the flag. Keyed by path — the crate's tests
/// run in parallel, and a global flag would let an unrelated test's
/// reindex consume the injection.
#[cfg(test)]
pub(crate) static FAIL_NEXT_FINISH: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// Capture the pre-edit state of a file edit, BEFORE the caller deletes
/// the file's old rows. Call [`finish_file_edit`] after the new rows are
/// written, inside the same transaction.
pub(crate) fn begin_file_edit(tx: &rusqlite::Transaction, rel_path: &str) -> Result<FileEditScope> {
    if !table_fresh(tx)? {
        return Ok(FileEditScope {
            rel_path: rel_path.to_string(),
            a_pre: Vec::new(),
            old_canonical_ids: Vec::new(),
            predecessor_ids: Vec::new(),
            skipped: true,
        });
    }

    let mut a_pre = affected_names(tx, rel_path)?;
    a_pre.sort();
    a_pre.dedup();

    let mut old_canonical_ids: Vec<i64> = canonical_ids_for_names(tx, &a_pre)?
        .values()
        .copied()
        .collect();
    old_canonical_ids.sort_unstable();
    old_canonical_ids.dedup();

    let mut predecessor_ids = sources_targeting_names(tx, &a_pre)?;
    predecessor_ids.sort_unstable();
    predecessor_ids.dedup();

    Ok(FileEditScope {
        rel_path: rel_path.to_string(),
        a_pre,
        old_canonical_ids,
        predecessor_ids,
        skipped: false,
    })
}

/// Repair the reach table after a file edit, BEFORE the caller commits.
///
/// Deletes every row sourced from an affected id (pre- and post-edit
/// canonicals plus both reverse-lookup predecessor sets) and recomputes the
/// affected names and predecessor sources from the post-edit graph through
/// the same [`compute_source_rows`] traversal the full build uses, at the
/// table's own `built_depth` and the default fan-out cap. Depth and
/// staleness meta are left untouched.
///
/// On error the caller must [`mark_stale`] in the same transaction and
/// commit anyway (REQ-007): reach is a cache, and a stale table falls back
/// to BFS rather than serving wrong data.
pub(crate) fn finish_file_edit(
    tx: &rusqlite::Transaction,
    scope: &FileEditScope,
) -> Result<ReachRepairStats> {
    if scope.skipped {
        return Ok(ReachRepairStats {
            skipped: true,
            ..ReachRepairStats::default()
        });
    }

    #[cfg(test)]
    {
        let mut fail_for = FAIL_NEXT_FINISH.lock().unwrap();
        if fail_for.as_deref() == Some(scope.rel_path.as_str()) {
            *fail_for = None;
            return Err(anyhow::anyhow!("injected finish_file_edit failure"));
        }
    }

    // Post-edit affected names, unioned with the pre-edit set.
    let mut a_all = affected_names(tx, &scope.rel_path)?;
    a_all.extend(scope.a_pre.iter().cloned());
    a_all.sort();
    a_all.dedup();

    // Post-edit predecessors over the union set, still against the
    // untouched table (rows deleted below).
    let mut predecessor_ids = scope.predecessor_ids.clone();
    predecessor_ids.extend(sources_targeting_names(tx, &a_all)?);
    predecessor_ids.sort_unstable();
    predecessor_ids.dedup();

    // Names to recompute: every affected name, plus the names of the
    // predecessor sources — their traversals pass through an affected
    // name, so their rows may change even though the names themselves are
    // untouched by the edit.
    let mut rebuild_names = a_all;
    rebuild_names.extend(names_for_ids(tx, &predecessor_ids)?);
    rebuild_names.sort();
    rebuild_names.dedup();

    // Affected source ids: pre-edit canonicals (rows keyed under ids that
    // may stop being canonical), post-edit canonicals, and predecessors.
    let post_canonical = canonical_ids_for_names(tx, &rebuild_names)?;
    let mut affected_ids: Vec<i64> = scope.old_canonical_ids.clone();
    affected_ids.extend(post_canonical.values().copied());
    affected_ids.extend(predecessor_ids.iter().copied());
    affected_ids.sort_unstable();
    affected_ids.dedup();

    let mut rows_deleted = 0usize;
    for chunk in affected_ids.chunks(IN_CHUNK) {
        let placeholders = vec!["?"; chunk.len()].join(", ");
        let sql = format!("DELETE FROM reach WHERE source_id IN ({placeholders})");
        rows_deleted += tx.execute(&sql, rusqlite::params_from_iter(chunk.iter()))?;
        let sql = format!("DELETE FROM reach_truncated WHERE source_id IN ({placeholders})");
        tx.execute(&sql, rusqlite::params_from_iter(chunk.iter()))?;
    }

    // The table self-describes: repair runs at the recorded built depth
    // under the default fan-out cap. No config plumbing.
    let built: usize = tx
        .query_row(
            "SELECT value FROM reach_meta WHERE key = ?1",
            rusqlite::params![META_BUILT_DEPTH],
            |row| row.get::<_, String>(0),
        )
        .context("reading built_depth for repair")?
        .parse()
        .context("parsing built_depth for repair")?;
    let opts = ReachBuildOptions {
        depth: built,
        max_targets: DEFAULT_MAX_TARGETS_PER_SOURCE,
    };

    let mut rows: Vec<(i64, i64, i64, f64)> = Vec::new();
    let mut truncated_sources: Vec<i64> = Vec::new();
    let mut rebuilt_sources = 0usize;
    {
        let mut candidates = SqlCandidates::new(tx)?;
        for name in &rebuild_names {
            let Some(&source_id) = post_canonical.get(name) else {
                // The name lost eligibility (deleted or Module-only now):
                // its rows are gone and lookups fall back to BFS.
                continue;
            };
            rebuilt_sources += 1;
            let (source_rows, truncated) = compute_source_rows(&mut candidates, name, &opts)?;
            if truncated {
                truncated_sources.push(source_id);
            }
            rows.extend(
                source_rows
                    .into_iter()
                    .map(|(target_id, min_depth, confidence)| {
                        (source_id, target_id, min_depth, confidence)
                    }),
            );
        }
    }

    write_reach_rows(tx, &rows)?;
    for source_id in &truncated_sources {
        tx.execute(
            "INSERT OR REPLACE INTO reach_truncated (source_id) VALUES (?1)",
            rusqlite::params![source_id],
        )?;
    }

    Ok(ReachRepairStats {
        rebuilt_sources,
        rows_deleted,
        rows_written: rows.len(),
        skipped: false,
    })
}

/// Whether the reach table can be incrementally repaired: present, built
/// (numeric `built_depth`), and not stale.
fn table_fresh(conn: &Connection) -> Result<bool> {
    let exists: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='reach'",
        [],
        |row| row.get(0),
    )?;
    if exists == 0 {
        return Ok(false);
    }
    let built: Option<String> = conn
        .query_row(
            "SELECT value FROM reach_meta WHERE key = ?1",
            rusqlite::params![META_BUILT_DEPTH],
            |row| row.get(0),
        )
        .ok();
    if built.and_then(|v| v.parse::<usize>().ok()).is_none() {
        return Ok(false);
    }
    let stale: i64 = conn.query_row(
        "SELECT COUNT(*) FROM reach_meta WHERE key = ?1",
        rusqlite::params![META_STALE],
        |row| row.get(0),
    )?;
    Ok(stale == 0)
}

/// Names whose candidate lists or symbol sets a file edit can change: the
/// file's symbol names, the callee names of its references with a resolved
/// caller, and the parent names of type edges whose child is in the file.
fn affected_names(conn: &Connection, rel_path: &str) -> Result<Vec<String>> {
    let mut names = Vec::new();
    for sql in [
        "SELECT DISTINCT name FROM symbols WHERE file = ?1",
        "SELECT DISTINCT name FROM \"references\" WHERE file = ?1 AND caller_id IS NOT NULL",
        "SELECT DISTINCT p.name FROM type_edges te \
         JOIN symbols p ON p.id = te.parent_id \
         JOIN symbols c ON c.id = te.child_id \
         WHERE c.file = ?1",
    ] {
        let mut stmt = conn.prepare(sql)?;
        let rows = stmt.query_map(rusqlite::params![rel_path], |row| row.get::<_, String>(0))?;
        for row in rows {
            names.push(row?);
        }
    }
    Ok(names)
}

/// Canonical (MIN-eligible) source id per name, for the names that have
/// one — the same rule the build and lookup key rows under.
fn canonical_ids_for_names(conn: &Connection, names: &[String]) -> Result<HashMap<String, i64>> {
    let mut ids = HashMap::new();
    for chunk in names.chunks(IN_CHUNK) {
        if chunk.is_empty() {
            continue;
        }
        let placeholders = vec!["?"; chunk.len()].join(", ");
        let sql = format!(
            "SELECT name, MIN(id) FROM symbols \
             WHERE kind <> 'module' AND name IN ({placeholders}) GROUP BY name"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(chunk.iter()), |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?;
        for row in rows {
            let (name, id) = row?;
            ids.insert(name, id);
        }
    }
    Ok(ids)
}

/// Sources holding rows that target a symbol named in `names` — the
/// reverse `target_id` lookup REQ-005 prescribes, served by
/// `idx_reach_target`. One hop suffices: a traversal expands a name only
/// after recording a row targeting a symbol of that name, so roots plus
/// this lookup cover every source whose result the edit can move.
fn sources_targeting_names(conn: &Connection, names: &[String]) -> Result<Vec<i64>> {
    let mut ids = Vec::new();
    for chunk in names.chunks(IN_CHUNK) {
        if chunk.is_empty() {
            continue;
        }
        let placeholders = vec!["?"; chunk.len()].join(", ");
        let sql = format!(
            "SELECT DISTINCT source_id FROM reach \
             WHERE target_id IN (SELECT id FROM symbols WHERE name IN ({placeholders}))"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(chunk.iter()), |row| {
            row.get::<_, i64>(0)
        })?;
        for row in rows {
            ids.push(row?);
        }
    }
    Ok(ids)
}

/// Names of the symbols carrying the given ids. Ids without a symbol
/// (impossible under FK cascade, possible in hand-built databases) drop
/// out silently — they have no name to recompute.
fn names_for_ids(conn: &Connection, ids: &[i64]) -> Result<Vec<String>> {
    let mut names = Vec::new();
    for chunk in ids.chunks(IN_CHUNK) {
        if chunk.is_empty() {
            continue;
        }
        let placeholders = vec!["?"; chunk.len()].join(", ");
        let sql = format!("SELECT DISTINCT name FROM symbols WHERE id IN ({placeholders})");
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(chunk.iter()), |row| {
            row.get::<_, String>(0)
        })?;
        for row in rows {
            names.push(row?);
        }
    }
    Ok(names)
}

/// Chunked multi-VALUES insert of reach rows, shared by the full build and
/// the incremental repair.
fn write_reach_rows(tx: &rusqlite::Transaction, rows: &[(i64, i64, i64, f64)]) -> Result<()> {
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
pub(crate) struct ReachGraph {
    /// All symbols (any kind — Modules can be targets), ordered by id.
    symbols: Vec<LoadedSymbol>,
    /// Eligibility (non-Module) per symbol position.
    eligible: Vec<bool>,
    /// name -> positions into `symbols` (all kinds).
    by_name: HashMap<String, Vec<usize>>,
    /// callee name -> caller candidates, one per caller symbol row carrying
    /// that row's MAX confidence among its refs to the name, ordered by
    /// (file, line, id) — finalized at load.
    refs_by_name: HashMap<String, Vec<ReachCandidate>>,
    /// parent name -> child candidates (union over same-named parents,
    /// deduplicated per child row), ordered by (file, line, id) — finalized
    /// at load.
    children_by_parent_name: HashMap<String, Vec<ReachCandidate>>,
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

        let candidate = |pos: &usize| -> ReachCandidate {
            let sym = &symbols[*pos];
            ReachCandidate {
                id: sym.id,
                name: sym.name.clone(),
                kind: sym.kind,
                file: sym.file.clone(),
                line: sym.line,
                confidence: 0.0,
            }
        };

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

        // The candidate lists are pure functions of the immutable graph, so
        // they are finalized once here instead of per BFS step: fold each
        // callee's refs to one row per caller (MAX confidence), dedup the
        // children lists per child row, and sort both by (file, line, id) —
        // the deterministic representative rules shared with blast's ordered
        // traversal and with `SqlCandidates`'s SQL (id breaks exact
        // (file, line) ties, which the DB layer permits).
        let order_by_location = |a: &ReachCandidate, b: &ReachCandidate| {
            a.file
                .cmp(&b.file)
                .then(a.line.cmp(&b.line))
                .then(a.id.cmp(&b.id))
        };
        let refs_by_name: HashMap<String, Vec<ReachCandidate>> = refs_by_name
            .into_iter()
            .map(|(name, mut candidates)| {
                let mut max_conf: HashMap<usize, f64> = HashMap::with_capacity(candidates.len());
                for (caller_pos, confidence) in candidates.iter() {
                    let slot = max_conf.entry(*caller_pos).or_insert(*confidence);
                    if *confidence > *slot {
                        *slot = *confidence;
                    }
                }
                candidates = max_conf.into_iter().collect();
                let mut folded: Vec<ReachCandidate> = candidates
                    .iter()
                    .map(|(pos, conf)| {
                        let mut cand = candidate(pos);
                        cand.confidence = *conf;
                        cand
                    })
                    .collect();
                folded.sort_by(order_by_location);
                (name, folded)
            })
            .collect();
        let children_by_parent_name: HashMap<String, Vec<ReachCandidate>> = children_by_parent_name
            .into_iter()
            .map(|(name, positions)| {
                // Dedup per child row: same-named parents (or duplicate
                // edges) can list one child twice; the traversal's
                // (name, file) dedup would drop the repeat anyway.
                let mut seen = HashSet::new();
                let mut children: Vec<ReachCandidate> = positions
                    .iter()
                    .filter(|pos| seen.insert(**pos))
                    .map(|pos| {
                        let mut cand = candidate(pos);
                        cand.confidence = 1.0;
                        cand
                    })
                    .collect();
                children.sort_by(order_by_location);
                (name, children)
            })
            .collect();

        Ok(Self {
            symbols,
            eligible,
            by_name,
            refs_by_name,
            children_by_parent_name,
        })
    }

    /// Canonical source id for `name`: MIN id over its eligible symbols, or
    /// `None` for unknown and Module-only names.
    fn canonical_source_id(&self, name: &str) -> Option<i64> {
        let positions = self.by_name.get(name)?;
        positions
            .iter()
            .copied()
            .filter(|&p| self.eligible[p])
            .map(|p| self.symbols[p].id)
            .min()
    }

    /// Candidates calling `name` — precomputed at load, so traversal only
    /// looks it up.
    fn caller_candidates(&self, name: &str) -> &[ReachCandidate] {
        self.refs_by_name.get(name).map_or(&[], |v| v.as_slice())
    }

    /// Type-edge children of any symbol named `name` — precomputed at load.
    fn child_candidates(&self, name: &str) -> &[ReachCandidate] {
        self.children_by_parent_name
            .get(name)
            .map_or(&[], |v| v.as_slice())
    }
}

/// [`CandidateSource`] over the loaded [`ReachGraph`] — the full build's
/// engine.
pub(crate) struct GraphCandidates<'a> {
    graph: &'a ReachGraph,
}

impl<'a> GraphCandidates<'a> {
    pub(crate) fn new(graph: &'a ReachGraph) -> Self {
        Self { graph }
    }
}

impl CandidateSource for GraphCandidates<'_> {
    fn caller_candidates(&mut self, name: &str) -> Result<&[ReachCandidate]> {
        Ok(self.graph.caller_candidates(name))
    }

    fn child_candidates(&mut self, name: &str) -> Result<&[ReachCandidate]> {
        Ok(self.graph.child_candidates(name))
    }
}

/// [`CandidateSource`] over indexed SQL — the incremental repair's engine.
///
/// The two prepared statements fold exactly like [`ReachGraph::load`]: one
/// row per caller symbol id with the MAX confidence among that caller's
/// refs to the name (`GROUP BY s.id` + `MAX`), one row per child symbol id
/// (`GROUP BY c.id`), both ordered by (file, line, id). A per-repair memo
/// caches lookups so rebuilding many sources in one repair reuses each
/// name's candidate list.
pub(crate) struct SqlCandidates<'a> {
    caller_stmt: rusqlite::Statement<'a>,
    child_stmt: rusqlite::Statement<'a>,
    caller_memo: HashMap<String, Vec<ReachCandidate>>,
    child_memo: HashMap<String, Vec<ReachCandidate>>,
}

impl<'a> SqlCandidates<'a> {
    pub(crate) fn new(conn: &'a Connection) -> Result<Self> {
        let caller_stmt = conn.prepare(
            "SELECT s.id, s.name, s.kind, s.file, s.line, MAX(r.confidence) \
             FROM \"references\" r JOIN symbols s ON s.id = r.caller_id \
             WHERE r.name = ?1 \
             GROUP BY s.id ORDER BY s.file, s.line, s.id",
        )?;
        let child_stmt = conn.prepare(
            "SELECT c.id, c.name, c.kind, c.file, c.line \
             FROM type_edges te \
             JOIN symbols p ON p.id = te.parent_id \
             JOIN symbols c ON c.id = te.child_id \
             WHERE p.name = ?1 \
             GROUP BY c.id ORDER BY c.file, c.line, c.id",
        )?;
        Ok(Self {
            caller_stmt,
            child_stmt,
            caller_memo: HashMap::new(),
            child_memo: HashMap::new(),
        })
    }

    fn map_caller_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ReachCandidate> {
        Ok(ReachCandidate {
            id: row.get(0)?,
            name: row.get(1)?,
            kind: SymbolKind::from_str(&row.get::<_, String>(2)?).unwrap_or(SymbolKind::Function),
            file: row.get(3)?,
            line: row.get(4)?,
            confidence: row.get(5)?,
        })
    }

    fn map_child_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ReachCandidate> {
        Ok(ReachCandidate {
            id: row.get(0)?,
            name: row.get(1)?,
            kind: SymbolKind::from_str(&row.get::<_, String>(2)?).unwrap_or(SymbolKind::Function),
            file: row.get(3)?,
            line: row.get(4)?,
            confidence: 1.0,
        })
    }
}

impl CandidateSource for SqlCandidates<'_> {
    fn caller_candidates(&mut self, name: &str) -> Result<&[ReachCandidate]> {
        if !self.caller_memo.contains_key(name) {
            let rows = self
                .caller_stmt
                .query_map(rusqlite::params![name], Self::map_caller_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            self.caller_memo.insert(name.to_string(), rows);
        }
        Ok(&self.caller_memo[name])
    }

    fn child_candidates(&mut self, name: &str) -> Result<&[ReachCandidate]> {
        if !self.child_memo.contains_key(name) {
            let rows = self
                .child_stmt
                .query_map(rusqlite::params![name], Self::map_child_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            self.child_memo.insert(name.to_string(), rows);
        }
        Ok(&self.child_memo[name])
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

    // Deterministic source-name order keeps row insertion stable.
    let mut names: Vec<&String> = graph.by_name.keys().collect();
    names.sort();

    let mut rows: Vec<(i64, i64, i64, f64)> = Vec::new();
    let mut truncated_sources: Vec<i64> = Vec::new();
    let mut sources = 0usize;

    let mut candidates = GraphCandidates::new(&graph);
    for name in names {
        let Some(source_id) = graph.canonical_source_id(name) else {
            continue; // Module-only names are not precomputation targets.
        };
        sources += 1;

        let (source_rows, truncated) = compute_source_rows(&mut candidates, name, opts)?;
        if truncated {
            truncated_sources.push(source_id);
        }
        rows.extend(
            source_rows
                .into_iter()
                .map(|(target_id, min_depth, confidence)| {
                    (source_id, target_id, min_depth, confidence)
                }),
        );
    }

    // Phase 3: replace previous contents inside the caller's transaction.
    tx.execute("DELETE FROM reach", [])?;
    tx.execute("DELETE FROM reach_truncated", [])?;
    tx.execute("DELETE FROM reach_meta", [])?;

    rows.sort_unstable_by_key(|r| (r.0, r.1));
    write_reach_rows(tx, &rows)?;

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

/// Shared AR-021 oracle: on a fresh, built table, every eligible name must
/// be answered with exactly the live BFS result at every depth up to the
/// built depth, and nothing else may be answered.
///
/// One helper, three suites: the TASK-080 build equivalence suite and the
/// TASK-081 incremental-maintenance suites (unit and pipeline-level) all
/// assert the same contract through it.
#[cfg(test)]
pub(crate) fn assert_table_equivalent_to_bfs(conn: &Connection) {
    let stale: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM reach_meta WHERE key = ?1",
            rusqlite::params![META_STALE],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(stale, 0, "reach table must not be stale");

    let built: usize = conn
        .query_row(
            "SELECT value FROM reach_meta WHERE key = ?1",
            rusqlite::params![META_BUILT_DEPTH],
            |row| row.get::<_, String>(0),
        )
        .unwrap()
        .parse()
        .expect("built_depth must be numeric");

    let mut names: Vec<String> = {
        let mut stmt = conn
            .prepare("SELECT DISTINCT name FROM symbols ORDER BY name")
            .unwrap();
        stmt.query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    };
    names.push("definitely_missing_name".into());

    for name in &names {
        let eligible: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM symbols WHERE name = ?1 AND kind <> 'module'",
                rusqlite::params![name],
                |row| row.get(0),
            )
            .unwrap();
        for depth in 1..=built {
            let where_ = format!("name {name} depth {depth}");
            let options = |use_reach: bool| crate::blast::BlastOptions {
                depth,
                direction: crate::types::BlastDirection::Upstream,
                include_tests: false,
                min_confidence: None,
                use_reach,
            };

            let bfs = crate::blast::analyze_blast(conn, name, &options(false)).unwrap();
            let routed = crate::blast::analyze_blast(conn, name, &options(true)).unwrap();
            let answer = lookup_upstream(conn, name, depth).unwrap();

            if eligible > 0 {
                let answer =
                    answer.unwrap_or_else(|| panic!("{where_}: table must cover the name"));
                assert!(!answer.truncated, "{where_}: uncapped table truncated");
                assert_eq!(
                    answer.affected.len(),
                    bfs.total_affected,
                    "{where_}: row count vs BFS total"
                );
                assert_eq!(
                    routed, bfs,
                    "{where_}: routed (table) result must equal the BFS result"
                );
            } else {
                assert!(
                    answer.is_none(),
                    "{where_}: Module-only/unknown names must not be answered"
                );
                assert_eq!(routed, bfs, "{where_}: fallback must be the exact BFS");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;
    use std::fs;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
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

    fn reach_row_count(conn: &Connection) -> i64 {
        conn.query_row("SELECT COUNT(*) FROM reach", [], |row| row.get(0))
            .unwrap()
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

    // -- Shared traversal core (NameBfs) -------------------------------------

    #[test]
    fn name_bfs_seeds_root_and_pops_fifo() {
        let mut bfs = NameBfs::new("root");
        assert_eq!(bfs.pop(), Some(("root".to_string(), 1)));
        assert_eq!(bfs.pop(), None, "single seed entry");
    }

    #[test]
    fn name_bfs_admit_dedups_on_name_and_file() {
        let mut bfs = NameBfs::new("root");
        bfs.pop();

        // Same (name, file) is admitted once.
        assert!(bfs.admit("a", "src/a.rs", 1, 3));
        assert!(!bfs.admit("a", "src/a.rs", 1, 3), "duplicate (name, file)");

        // Same name in another file, and another name in the same file, are
        // distinct symbol rows: both admit.
        assert!(bfs.admit("a", "src/b.rs", 1, 3));
        assert!(bfs.admit("b", "src/a.rs", 1, 3));
    }

    #[test]
    fn name_bfs_admit_enqueues_only_under_the_depth_cap() {
        let mut bfs = NameBfs::new("root");
        bfs.pop();

        // At the cap (depth == max_depth): recorded, never expanded.
        assert!(bfs.admit("deep", "src/a.rs", 3, 3));
        assert_eq!(bfs.pop(), None, "depth == max_depth must not enqueue");

        // Under the cap: expanded at depth + 1.
        assert!(bfs.admit("shallow", "src/a.rs", 1, 3));
        assert_eq!(bfs.pop(), Some(("shallow".to_string(), 2)));
    }

    #[test]
    fn name_bfs_admit_never_requeues_a_name() {
        let mut bfs = NameBfs::new("root");
        bfs.pop();

        // Two distinct-file symbols of one name both record, but the name is
        // enqueued for expansion exactly once.
        assert!(bfs.admit("x", "src/a.rs", 1, 3));
        assert!(bfs.admit("x", "src/b.rs", 1, 3));
        assert_eq!(bfs.pop(), Some(("x".to_string(), 2)));
        assert_eq!(bfs.pop(), None, "one name, one expansion");
    }

    #[test]
    fn name_bfs_root_name_is_never_requeued() {
        let mut bfs = NameBfs::new("root");
        bfs.pop();

        // A cycle back to the queried name records the self-row but does not
        // re-expand the root (it was queued at seeding).
        assert!(bfs.admit("root", "src/a.rs", 1, 3));
        assert_eq!(bfs.pop(), None);
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

    // -- Equivalence suite (AR-021) -------------------------------------------
    //
    // The table is equivalent to the BFS by construction (one shared
    // predicate, mirrored traversal); this suite guards against drift.

    /// Inline splitmix64 — deterministic per-seed graph generation without an
    /// external rng dependency.
    struct SplitMix64(u64);

    impl SplitMix64 {
        fn next_u64(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        }

        fn below(&mut self, n: usize) -> usize {
            (self.next_u64() % n as u64) as usize
        }
    }

    const EQUIV_KINDS: [&str; 11] = [
        "function",
        "method",
        "class",
        "struct",
        "interface",
        "enum",
        "trait",
        "type_alias",
        "constant",
        "variable",
        "module",
    ];

    const EQUIV_FILES: [&str; 5] = [
        "src/a.rs",
        "src/b.rs",
        "src/c.rs",
        "tests/t1.rs",
        "tests/t2.rs",
    ];

    const EQUIV_CONFS: [f64; 4] = [0.5, 0.8, 0.85, 0.95];

    /// Small name pool: collisions across kinds and files are guaranteed.
    const EQUIV_POOL: [&str; 24] = [
        "alpha", "bravo", "charlie", "delta", "echo", "foxtrot", "golf", "hotel", "india",
        "juliet", "kilo", "lima", "mike", "november", "oscar", "papa", "quebec", "romeo", "sierra",
        "tango", "uniform", "victor", "whiskey", "xray",
    ];

    /// Names the structured part of the graph also exposes as call targets,
    /// so random callers cross into the deterministic structures.
    const EQUIV_STRUCTURED: [&str; 6] = ["shared", "poly", "hub_t", "chain_2", "d_bot", "modonly"];

    /// Inserts symbols with per-file unique line numbers (deterministic
    /// candidate ordering in both traversal engines).
    struct GraphBuilder<'a> {
        conn: &'a Connection,
        lines: HashMap<String, i64>,
    }

    impl<'a> GraphBuilder<'a> {
        fn new(conn: &'a Connection) -> Self {
            Self {
                conn,
                lines: HashMap::new(),
            }
        }

        fn symbol(&mut self, name: &str, kind: &str, file: &str) -> i64 {
            let line = {
                let slot = self.lines.entry(file.to_string()).or_insert(0);
                *slot += 1;
                *slot
            };
            insert_symbol(self.conn, name, kind, file, line)
        }
    }

    /// Populate one seed's synthetic graph: ~120 random background symbols on
    /// top of deterministic structures — a 5-link chain (depth > 3), a
    /// diamond, mutual-recursion and self-call cycles, a 15-caller hub (with
    /// test-file callers), same-name symbols across files, duplicate
    /// (name, file) rows, a multi-call-site caller, a Module-only name, a
    /// kind-colliding name, and type edges including same-named parents.
    fn seed_graph(conn: &Connection, seed: u64) {
        let mut rng = SplitMix64(seed);
        let mut g = GraphBuilder::new(conn);

        // Chain longer than the built depth: chain_i calls chain_{i+1}.
        for i in 0..5 {
            g.symbol(&format!("chain_{i}"), "function", EQUIV_FILES[i % 3]);
        }
        for i in 0..4 {
            let caller: i64 = conn
                .query_row(
                    "SELECT id FROM symbols WHERE name = ?1",
                    rusqlite::params![format!("chain_{i}")],
                    |row| row.get(0),
                )
                .unwrap();
            insert_ref(conn, &format!("chain_{}", i + 1), Some(caller), 0.85);
        }

        // Diamond: d_top -> {d_l, d_r} -> d_bot.
        let d_top = g.symbol("d_top", "function", "src/a.rs");
        let d_l = g.symbol("d_l", "function", "src/b.rs");
        let d_r = g.symbol("d_r", "method", "src/b.rs");
        g.symbol("d_bot", "class", "src/c.rs");
        insert_ref(conn, "d_l", Some(d_top), 0.95);
        insert_ref(conn, "d_r", Some(d_top), 0.8);
        insert_ref(conn, "d_bot", Some(d_l), 0.85);
        insert_ref(conn, "d_bot", Some(d_r), 0.95);

        // Cycles: mutual recursion and a self-call.
        let cyc_a = g.symbol("cyc_a", "function", "src/a.rs");
        let cyc_b = g.symbol("cyc_b", "function", "src/b.rs");
        insert_ref(conn, "cyc_b", Some(cyc_a), 0.85);
        insert_ref(conn, "cyc_a", Some(cyc_b), 0.85);
        let self_x = g.symbol("self_x", "function", "src/a.rs");
        insert_ref(conn, "self_x", Some(self_x), 0.85);

        // Hub with 15 callers, a few discovered in test files.
        g.symbol("hub_t", "function", "src/a.rs");
        for i in 0..15 {
            let file = if i % 5 == 4 {
                "tests/t1.rs"
            } else {
                EQUIV_FILES[i % 3]
            };
            let caller = g.symbol(&format!("hub_c{i}"), "function", file);
            insert_ref(conn, "hub_t", Some(caller), EQUIV_CONFS[i % 4]);
        }

        // Same name across files, one landing in a test file.
        let shared_ids: Vec<i64> = ["src/a.rs", "src/b.rs", "tests/t1.rs"]
            .iter()
            .map(|f| g.symbol("shared", "function", f))
            .collect();
        // Duplicate (name, file): two `dup` rows in one file, both calling shared.
        for _ in 0..2 {
            let dup = g.symbol("dup", "function", "src/a.rs");
            insert_ref(conn, "shared", Some(dup), 0.8);
        }
        // Multi-call-site caller: three refs to shared at different confidences.
        let mcs = g.symbol("mcs", "method", "src/b.rs");
        for conf in EQUIV_CONFS {
            insert_ref(conn, "shared", Some(mcs), conf);
        }
        // Module-only name with a caller (target coverage, never a source).
        g.symbol("modonly", "module", "src/a.rs");
        let mod_caller = g.symbol("mod_caller", "function", "src/c.rs");
        insert_ref(conn, "modonly", Some(mod_caller), 0.95);
        // Kind collision: struct + function under one name, each with a
        // type-edge child (children resolve by parent NAME union).
        let poly_struct = g.symbol("poly", "struct", "src/a.rs");
        let poly_fn = g.symbol("poly", "function", "src/b.rs");
        let poly_child_a = g.symbol("poly_child_a", "method", "src/a.rs");
        let poly_child_b = g.symbol("poly_child_b", "function", "src/c.rs");
        insert_type_edge(conn, poly_struct, poly_child_a);
        insert_type_edge(conn, poly_fn, poly_child_b);
        let poly_caller = g.symbol("poly_caller", "constant", "src/b.rs");
        insert_ref(conn, "poly", Some(poly_caller), 0.5);
        // Type-edge children under same-named parents in different files.
        for (i, &parent) in shared_ids.iter().enumerate() {
            let child = g.symbol(&format!("shared_child{i}"), "function", "src/c.rs");
            insert_type_edge(conn, parent, child);
        }

        // Random background: ~120 symbols, all 11 kinds, colliding names,
        // mixed files (incl. tests), NULL-caller refs, random out-degrees.
        let mut ids: Vec<i64> = Vec::new();
        for _ in 0..120 {
            let name = EQUIV_POOL[rng.below(EQUIV_POOL.len())];
            let kind = EQUIV_KINDS[rng.below(EQUIV_KINDS.len())];
            let file = EQUIV_FILES[rng.below(EQUIV_FILES.len())];
            let id = g.symbol(name, kind, file);
            ids.push(id);
        }
        for &id in &ids {
            let out = 1 + rng.below(4);
            for _ in 0..out {
                let callee = if rng.below(3) == 0 {
                    EQUIV_STRUCTURED[rng.below(EQUIV_STRUCTURED.len())]
                } else {
                    EQUIV_POOL[rng.below(EQUIV_POOL.len())]
                };
                let conf = EQUIV_CONFS[rng.below(EQUIV_CONFS.len())];
                // ~10% of refs have no resolved caller: invisible to both engines.
                let caller = if rng.below(10) == 0 { None } else { Some(id) };
                insert_ref(conn, callee, caller, conf);
            }
        }
        // Random type edges over the background symbols.
        for _ in 0..20 {
            let parent = ids[rng.below(ids.len())];
            let child = ids[rng.below(ids.len())];
            insert_type_edge(conn, parent, child);
        }
    }

    fn bfs_options(depth: usize, use_reach: bool) -> crate::blast::BlastOptions {
        crate::blast::BlastOptions {
            depth,
            direction: crate::types::BlastDirection::Upstream,
            include_tests: false,
            min_confidence: None,
            use_reach,
        }
    }

    fn distinct_names(conn: &Connection) -> Vec<String> {
        let mut stmt = conn
            .prepare("SELECT DISTINCT name FROM symbols ORDER BY name")
            .unwrap();
        stmt.query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    }

    /// AR-021 mandatory equivalence: for every name in every seeded graph,
    /// the table answer equals the live BFS at every depth ≤ built depth.
    #[test]
    fn equivalence_table_matches_bfs_on_random_graphs() {
        for seed in 1..=20u64 {
            let (_dir, conn) = make_db();
            seed_graph(&conn, seed);
            build(&conn, 3, usize::MAX);

            assert_table_equivalent_to_bfs(&conn);
        }
    }

    /// WI-1 parity seam: the in-memory candidate lists the full build walks
    /// and the SQL lists the incremental repair walks must be identical —
    /// this is the equivalence-by-construction guarantee the repair rests on.
    #[test]
    fn candidate_sources_agree_on_seeded_graphs() {
        for seed in 1..=5u64 {
            let (_dir, conn) = make_db();
            seed_graph(&conn, seed);
            let tx = conn.unchecked_transaction().unwrap();
            let graph = ReachGraph::load(&tx).unwrap();
            let names = distinct_names(&tx);
            {
                let mut from_graph = GraphCandidates::new(&graph);
                let mut from_sql = SqlCandidates::new(&tx).unwrap();

                for name in &names {
                    let g_callers = from_graph.caller_candidates(name).unwrap().to_vec();
                    let s_callers = from_sql.caller_candidates(name).unwrap().to_vec();
                    assert_eq!(
                        g_callers, s_callers,
                        "seed {seed} name {name}: caller candidate lists diverge"
                    );

                    let g_children = from_graph.child_candidates(name).unwrap().to_vec();
                    let s_children = from_sql.child_candidates(name).unwrap().to_vec();
                    assert_eq!(
                        g_children, s_children,
                        "seed {seed} name {name}: child candidate lists diverge"
                    );
                }
            }
            tx.commit().unwrap();
        }
    }

    /// Dimension canary: `include_tests` changes the shared predicate's
    /// verdict, so the BFS under that dimension disagrees with the table
    /// (which is built test-excluded). If the routing matrix ever let
    /// dimension queries hit the table, both engines would wrongly agree.
    #[test]
    fn equivalence_canary_include_tests_dimension_moves_bfs_not_table() {
        let (_dir, conn) = make_db();
        let victim = insert_symbol(&conn, "victim", "function", "src/a.rs", 1);
        let prod = insert_symbol(&conn, "prod_caller", "function", "src/b.rs", 2);
        let test = insert_symbol(&conn, "test_caller", "function", "tests/t1.rs", 3);
        insert_ref(&conn, "victim", Some(prod), 0.85);
        insert_ref(&conn, "victim", Some(test), 0.85);
        let _ = victim;
        build(&conn, 3, usize::MAX);

        let table = lookup_upstream(&conn, "victim", 3)
            .unwrap()
            .expect("covered");
        let table_names: HashSet<&str> = table.affected.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(
            table_names,
            HashSet::from(["prod_caller"]),
            "table excludes tests"
        );

        // Same dimension through BFS: the shared predicate now admits the
        // test caller, so the BFS result disagrees with the table answer.
        let bfs_tests = crate::blast::analyze_blast(
            &conn,
            "victim",
            &crate::blast::BlastOptions {
                include_tests: true,
                ..bfs_options(3, false)
            },
        )
        .unwrap();
        let bfs_names: HashSet<&str> = bfs_tests
            .tiers
            .iter()
            .flat_map(|t| t.symbols.iter().map(|s| s.name.as_str()))
            .collect();
        assert!(bfs_names.contains("test_caller"), "predicate moved the BFS");
        assert_ne!(bfs_names, table_names, "dimension result must differ");

        // Without the dimension both engines agree (the equivalence proper).
        let bfs_default =
            crate::blast::analyze_blast(&conn, "victim", &bfs_options(3, false)).unwrap();
        assert_eq!(bfs_default.total_affected, table.affected.len());
    }

    /// Dimension canary: `min_confidence` narrows the shared predicate, so
    /// the confidence-filtered BFS is a strict subset of the table answer.
    #[test]
    fn equivalence_canary_min_confidence_dimension_moves_bfs_not_table() {
        let (_dir, conn) = make_db();
        let victim = insert_symbol(&conn, "victim", "function", "src/a.rs", 1);
        let lo = insert_symbol(&conn, "lo_caller", "function", "src/b.rs", 2);
        let hi = insert_symbol(&conn, "hi_caller", "function", "src/c.rs", 3);
        insert_ref(&conn, "victim", Some(lo), 0.5);
        insert_ref(&conn, "victim", Some(hi), 0.95);
        let _ = victim;
        build(&conn, 3, usize::MAX);

        let table = lookup_upstream(&conn, "victim", 3)
            .unwrap()
            .expect("covered");
        let table_names: HashSet<&str> = table.affected.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(table_names, HashSet::from(["lo_caller", "hi_caller"]));

        let bfs_strict = crate::blast::analyze_blast(
            &conn,
            "victim",
            &crate::blast::BlastOptions {
                min_confidence: Some(0.9),
                ..bfs_options(3, false)
            },
        )
        .unwrap();
        let strict_names: HashSet<&str> = bfs_strict
            .tiers
            .iter()
            .flat_map(|t| t.symbols.iter().map(|s| s.name.as_str()))
            .collect();
        assert_eq!(
            strict_names,
            HashSet::from(["hi_caller"]),
            "predicate moved the BFS"
        );
        assert!(
            strict_names.is_subset(&table_names),
            "filtered BFS is a subset, never equal here"
        );
    }
    // -- Incremental maintenance suite (TASK-081, PRD-REACH-REQ-005/007) ----
    //
    // Every test simulates a file edit exactly as `upsert_file_data` /
    // `delete_file_data` perform it: begin -> delete old rows -> insert new
    // rows -> finish -> commit, in one transaction. The strongest oracle is
    // `assert_incremental_equals_full_rebuild`: the incrementally
    // maintained table must equal a from-scratch rebuild, row for row.

    /// A file's post-edit content, in the shape `upsert_file_data` writes.
    struct FileSpec {
        /// (name, kind); line numbers assigned 1..=n in order.
        symbols: Vec<(String, String)>,
        /// (callee name, caller symbol name in this file, confidence).
        refs: Vec<(String, Option<String>, f64)>,
        /// (parent name, child name); child resolved in-file, parent
        /// in-file first then cross-file.
        type_edges: Vec<(String, String)>,
    }

    fn spec(
        symbols: Vec<(&str, &str)>,
        refs: Vec<(&str, Option<&str>, f64)>,
        type_edges: Vec<(&str, &str)>,
    ) -> FileSpec {
        FileSpec {
            symbols: symbols
                .into_iter()
                .map(|(n, k)| (n.to_string(), k.to_string()))
                .collect(),
            refs: refs
                .into_iter()
                .map(|(n, c, f)| (n.to_string(), c.map(|s| s.to_string()), f))
                .collect(),
            type_edges: type_edges
                .into_iter()
                .map(|(p, c)| (p.to_string(), c.to_string()))
                .collect(),
        }
    }

    /// Simulate `upsert_file_data`: begin, delete the file's old rows,
    /// insert the new rows, finish, commit — one transaction.
    fn apply_edit(conn: &Connection, file: &str, edit: &FileSpec) -> ReachRepairStats {
        let tx = conn.unchecked_transaction().unwrap();
        let scope = begin_file_edit(&tx, file).unwrap();

        tx.execute(
            "DELETE FROM type_edges WHERE child_id IN (SELECT id FROM symbols WHERE file = ?1)",
            rusqlite::params![file],
        )
        .unwrap();
        tx.execute(
            "DELETE FROM symbols WHERE file = ?1",
            rusqlite::params![file],
        )
        .unwrap();
        tx.execute(
            "DELETE FROM \"references\" WHERE file = ?1",
            rusqlite::params![file],
        )
        .unwrap();

        let mut ids: HashMap<String, i64> = HashMap::new();
        for (i, (name, kind)) in edit.symbols.iter().enumerate() {
            tx.execute(
                "INSERT INTO symbols (name, kind, file, line, col, language) \
                 VALUES (?1, ?2, ?3, ?4, 1, 'rust')",
                rusqlite::params![name, kind, file, (i + 1) as i64],
            )
            .unwrap();
            ids.insert(name.clone(), tx.last_insert_rowid());
        }
        for (i, (callee, caller, confidence)) in edit.refs.iter().enumerate() {
            let caller_id = caller.as_ref().and_then(|c| ids.get(c).copied());
            tx.execute(
                "INSERT INTO \"references\" (name, file, line, col, caller_id, confidence) \
                 VALUES (?1, ?2, ?3, 1, ?4, ?5)",
                rusqlite::params![callee, file, (i + 1) as i64, caller_id, confidence],
            )
            .unwrap();
        }
        for (parent, child) in &edit.type_edges {
            let Some(&child_id) = ids.get(child) else {
                continue;
            };
            let parent_id = ids.get(parent).copied().or_else(|| {
                tx.query_row(
                    "SELECT id FROM symbols WHERE name = ?1 LIMIT 1",
                    rusqlite::params![parent],
                    |row| row.get::<_, i64>(0),
                )
                .ok()
            });
            // Mirror upsert_file_data: unresolvable parents are skipped.
            let Some(parent_id) = parent_id else {
                continue;
            };
            tx.execute(
                "INSERT INTO type_edges (child_id, parent_id, relationship) \
                 VALUES (?1, ?2, 'impl')",
                rusqlite::params![child_id, parent_id],
            )
            .unwrap();
        }

        let stats = finish_file_edit(&tx, &scope).unwrap();
        tx.commit().unwrap();
        stats
    }

    /// Simulate `delete_file_data`: begin, delete the file's rows, finish,
    /// commit — one transaction, no new rows.
    fn apply_delete(conn: &Connection, file: &str) -> ReachRepairStats {
        let tx = conn.unchecked_transaction().unwrap();
        let scope = begin_file_edit(&tx, file).unwrap();
        tx.execute(
            "DELETE FROM type_edges WHERE child_id IN (SELECT id FROM symbols WHERE file = ?1)",
            rusqlite::params![file],
        )
        .unwrap();
        tx.execute(
            "DELETE FROM symbols WHERE file = ?1",
            rusqlite::params![file],
        )
        .unwrap();
        tx.execute(
            "DELETE FROM \"references\" WHERE file = ?1",
            rusqlite::params![file],
        )
        .unwrap();
        let stats = finish_file_edit(&tx, &scope).unwrap();
        tx.commit().unwrap();
        stats
    }

    fn symbol_id(conn: &Connection, name: &str) -> i64 {
        conn.query_row(
            "SELECT id FROM symbols WHERE name = ?1",
            rusqlite::params![name],
            |row| row.get(0),
        )
        .unwrap()
    }

    fn symbol_id_in(conn: &Connection, name: &str, file: &str) -> i64 {
        conn.query_row(
            "SELECT id FROM symbols WHERE name = ?1 AND file = ?2",
            rusqlite::params![name, file],
            |row| row.get(0),
        )
        .unwrap()
    }

    /// One reach row as snapshotted: (source, target, min_depth, confidence).
    type ReachRowSnapshot = (i64, i64, i64, f64);
    /// Whole-table snapshot: rows plus truncation markers.
    type ReachTableSnapshot = (Vec<ReachRowSnapshot>, std::collections::BTreeSet<i64>);

    /// Snapshot of the whole table: reach rows plus truncation markers.
    /// Rows are sorted for deterministic comparison (f64 is not `Ord`, so
    /// this is a sorted Vec, not a BTreeSet).
    fn snapshot_reach(conn: &Connection) -> ReachTableSnapshot {
        let mut rows: Vec<ReachRowSnapshot> = conn
            .prepare("SELECT source_id, target_id, min_depth, confidence FROM reach")
            .unwrap()
            .query_map([], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
            })
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        rows.sort_by(|a, b| {
            a.0.cmp(&b.0)
                .then(a.1.cmp(&b.1))
                .then(a.2.cmp(&b.2))
                .then(a.3.partial_cmp(&b.3).unwrap())
        });
        let truncated: std::collections::BTreeSet<i64> = conn
            .prepare("SELECT source_id FROM reach_truncated")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        (rows, truncated)
    }

    /// The incremental-maintenance oracle: snapshot the table, run a full
    /// rebuild with the repair's own options (built depth, default cap),
    /// and require row-for-row equality including truncation markers.
    /// Leaves the table in the rebuilt — by the assert, identical — state.
    fn assert_incremental_equals_full_rebuild(conn: &Connection) {
        let built: usize = built_depth(conn)
            .expect("table must be built")
            .parse()
            .unwrap();
        let incremental = snapshot_reach(conn);
        build(conn, built, DEFAULT_MAX_TARGETS_PER_SOURCE);
        let rebuilt = snapshot_reach(conn);
        assert_eq!(
            incremental, rebuilt,
            "incrementally maintained table must equal a full rebuild"
        );
    }

    fn assert_no_orphan_rows(conn: &Connection) {
        for column in ["source_id", "target_id"] {
            let orphans: i64 = conn
                .query_row(
                    &format!(
                        "SELECT COUNT(*) FROM reach r \
                         LEFT JOIN symbols s ON s.id = r.{column} WHERE s.id IS NULL"
                    ),
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(orphans, 0, "dangling {column} rows in reach");
        }
    }

    #[test]
    fn repair_adds_and_removes_rows_for_caller_edits() {
        let (_dir, conn) = make_db();
        apply_edit(
            &conn,
            "src/target.rs",
            &spec(vec![("target", "function")], vec![], vec![]),
        );
        apply_edit(
            &conn,
            "src/caller.rs",
            &spec(
                vec![("caller", "function")],
                vec![("target", Some("caller"), 0.9)],
                vec![],
            ),
        );
        build(&conn, 3, DEFAULT_MAX_TARGETS_PER_SOURCE);

        let target_id = symbol_id(&conn, "target");
        let caller_id = symbol_id(&conn, "caller");
        assert_eq!(reach_rows(&conn, target_id), vec![(caller_id, 1, 0.9)]);

        // Remove the call: target's rows drop to an authoritative empty.
        // (The old row dies via FK cascade when the caller symbol row is
        // replaced, so the repair's own delete count is 0 here — the
        // behavioral assert is the row's absence.)
        let stats = apply_edit(
            &conn,
            "src/caller.rs",
            &spec(vec![("caller", "function")], vec![], vec![]),
        );
        assert_eq!(stats.rows_written, 0, "nothing to write: no callers left");
        assert_eq!(reach_rows(&conn, target_id), vec![]);
        let answer = lookup_upstream(&conn, "target", 3)
            .unwrap()
            .expect("target is still an eligible, covered name");
        assert!(
            answer.affected.is_empty(),
            "covered-no-dependents is Some(empty), never silence"
        );
        assert_table_equivalent_to_bfs(&conn);
        assert_incremental_equals_full_rebuild(&conn);

        // Add the call back (new rowid for the caller symbol).
        apply_edit(
            &conn,
            "src/caller.rs",
            &spec(
                vec![("caller", "function")],
                vec![("target", Some("caller"), 0.95)],
                vec![],
            ),
        );
        let caller_id = symbol_id(&conn, "caller");
        assert_eq!(reach_rows(&conn, target_id), vec![(caller_id, 1, 0.95)]);
        assert_table_equivalent_to_bfs(&conn);
        assert_incremental_equals_full_rebuild(&conn);
    }

    #[test]
    fn repair_rebuilds_predecessors_via_reverse_target_lookup() {
        let (_dir, conn) = make_db();
        // deep <- mid <- top, one file per symbol.
        apply_edit(
            &conn,
            "src/deep.rs",
            &spec(vec![("deep", "function")], vec![], vec![]),
        );
        apply_edit(
            &conn,
            "src/mid.rs",
            &spec(
                vec![("mid", "function")],
                vec![("deep", Some("mid"), 0.9)],
                vec![],
            ),
        );
        apply_edit(
            &conn,
            "src/top.rs",
            &spec(
                vec![("top", "function")],
                vec![("mid", Some("top"), 0.9)],
                vec![],
            ),
        );
        build(&conn, 3, DEFAULT_MAX_TARGETS_PER_SOURCE);

        let deep_id = symbol_id(&conn, "deep");
        let mid_id = symbol_id(&conn, "mid");
        let top_id = symbol_id(&conn, "top");
        assert_eq!(
            reach_rows(&conn, deep_id),
            vec![(mid_id, 1, 0.9), (top_id, 2, 0.9)]
        );

        // top stops calling mid. "deep" appears in neither top.rs nor any
        // name top.rs touches — only the reverse lookup on target_id can
        // discover that deep's traversal passes through "mid" (REQ-005).
        let stats = apply_edit(
            &conn,
            "src/top.rs",
            &spec(vec![("top", "function")], vec![], vec![]),
        );
        assert!(
            stats.rebuilt_sources >= 1,
            "the predecessor source must be recomputed"
        );
        assert_eq!(
            reach_rows(&conn, deep_id),
            vec![(mid_id, 1, 0.9)],
            "top must drop out of deep's rows"
        );
        assert_table_equivalent_to_bfs(&conn);
        assert_incremental_equals_full_rebuild(&conn);
    }

    #[test]
    fn repair_rebuilds_predecessors_hidden_by_cascade() {
        let (_dir, conn) = make_db();
        // deep <- lowmid <- mid <- top, with BOTH mid and lowmid in one
        // file. deep's rows pass through mid and lowmid, whose symbol rows
        // an edit to their file deletes — FK cascade then removes deep's
        // linking rows ((deep, lowmid), (deep, mid)) before finish runs.
        // Two independent mechanisms must still find deep for the repair
        // to clear the stranded (deep, top) row: the pre-delete reverse
        // lookup (rows targeting mid/lowmid-named symbols), and the
        // ref-callee name "deep" in the affected set (the deleted ref in
        // mid.rs named deep). Remove either one alone and this still
        // passes; remove both and it fails — the redundancy is the point.
        apply_edit(
            &conn,
            "src/deep.rs",
            &spec(vec![("deep", "function")], vec![], vec![]),
        );
        apply_edit(
            &conn,
            "src/top.rs",
            &spec(
                vec![("top", "function")],
                vec![("mid", Some("top"), 0.9)],
                vec![],
            ),
        );
        apply_edit(
            &conn,
            "src/mid.rs",
            &spec(
                vec![("mid", "function"), ("lowmid", "function")],
                vec![("lowmid", Some("mid"), 0.9), ("deep", Some("lowmid"), 0.9)],
                vec![],
            ),
        );
        build(&conn, 3, DEFAULT_MAX_TARGETS_PER_SOURCE);

        let deep_id = symbol_id(&conn, "deep");
        let mid_id = symbol_id(&conn, "mid");
        let lowmid_id = symbol_id(&conn, "lowmid");
        let top_id = symbol_id(&conn, "top");
        assert_eq!(
            reach_rows(&conn, deep_id),
            // reach_rows orders by target_id: top(2)@3, mid(3)@2, lowmid(4)@1.
            vec![(top_id, 3, 0.9), (mid_id, 2, 0.9), (lowmid_id, 1, 0.9)]
        );

        // Delete the whole mid-chain file: deep's only callers vanish, so
        // its rows must drop to empty — including (deep, top), which only
        // the pre-cascade predecessor capture can reach.
        apply_delete(&conn, "src/mid.rs");
        assert_eq!(
            reach_rows(&conn, deep_id),
            vec![],
            "the cascaded linking rows must not strand (deep, top)"
        );
        assert_table_equivalent_to_bfs(&conn);
        assert_incremental_equals_full_rebuild(&conn);
    }

    #[test]
    fn repair_moves_rows_on_canonical_min_id_shift() {
        let (_dir, conn) = make_db();
        // Foo in two files; the lower id is canonical. c calls Foo.
        apply_edit(
            &conn,
            "src/a.rs",
            &spec(vec![("Foo", "struct")], vec![], vec![]),
        );
        apply_edit(
            &conn,
            "src/b.rs",
            &spec(vec![("Foo", "function")], vec![], vec![]),
        );
        apply_edit(
            &conn,
            "src/c.rs",
            &spec(
                vec![("c", "function")],
                vec![("Foo", Some("c"), 0.9)],
                vec![],
            ),
        );
        build(&conn, 3, DEFAULT_MAX_TARGETS_PER_SOURCE);

        let a_foo = symbol_id_in(&conn, "Foo", "src/a.rs");
        let b_foo = symbol_id_in(&conn, "Foo", "src/b.rs");
        let c_id = symbol_id(&conn, "c");
        assert_eq!(reach_rows(&conn, a_foo), vec![(c_id, 1, 0.9)]);
        assert!(reach_rows(&conn, b_foo).is_empty());

        // Delete the lower-id Foo: the canonical id shifts to b's Foo and
        // the rows must be re-keyed under it.
        apply_edit(&conn, "src/a.rs", &spec(vec![], vec![], vec![]));
        assert!(
            reach_rows(&conn, a_foo).is_empty(),
            "old canonical id must lose its rows"
        );
        assert_eq!(
            reach_rows(&conn, b_foo),
            vec![(c_id, 1, 0.9)],
            "rows re-keyed under the new canonical id"
        );
        let answer = lookup_upstream(&conn, "Foo", 3)
            .unwrap()
            .expect("Foo still has an eligible symbol");
        assert_eq!(answer.affected.len(), 1);
        assert_table_equivalent_to_bfs(&conn);
        assert_incremental_equals_full_rebuild(&conn);

        // Demote the remaining Foo to Module: the name loses eligibility,
        // its rows die, and lookups fall back to BFS.
        apply_edit(
            &conn,
            "src/b.rs",
            &spec(vec![("Foo", "module")], vec![], vec![]),
        );
        assert!(reach_rows(&conn, b_foo).is_empty());
        assert!(
            lookup_upstream(&conn, "Foo", 3).unwrap().is_none(),
            "module-only name must not be answered"
        );
        assert_table_equivalent_to_bfs(&conn);
        assert_incremental_equals_full_rebuild(&conn);
    }

    #[test]
    fn repair_delete_file_drops_rows_and_dangling_targets() {
        let (_dir, conn) = make_db();
        apply_edit(
            &conn,
            "src/deep.rs",
            &spec(vec![("deep", "function")], vec![], vec![]),
        );
        apply_edit(
            &conn,
            "src/mid.rs",
            &spec(
                vec![("mid", "function")],
                vec![("deep", Some("mid"), 0.9)],
                vec![],
            ),
        );
        apply_edit(
            &conn,
            "src/top.rs",
            &spec(
                vec![("top", "function")],
                vec![("mid", Some("top"), 0.9)],
                vec![],
            ),
        );
        build(&conn, 3, DEFAULT_MAX_TARGETS_PER_SOURCE);
        assert_no_orphan_rows(&conn);

        let stats = apply_delete(&conn, "src/mid.rs");
        assert!(!stats.skipped, "a fresh table is repairable on delete too");

        // No rows sourced from or targeting the deleted file's symbols:
        // cascade removes the referencing rows, the repair must not leave
        // anything behind (every row in this graph passed through mid).
        assert_no_orphan_rows(&conn);
        assert_eq!(reach_row_count(&conn), 0);
        assert_table_equivalent_to_bfs(&conn);
        assert_incremental_equals_full_rebuild(&conn);
    }

    #[test]
    fn repair_new_file_creates_sources() {
        let (_dir, conn) = make_db();
        apply_edit(
            &conn,
            "src/existing.rs",
            &spec(vec![("existing", "function")], vec![], vec![]),
        );
        // Edge-less graph: the build produces an empty but fresh table.
        build(&conn, 3, DEFAULT_MAX_TARGETS_PER_SOURCE);
        assert_eq!(reach_row_count(&conn), 0);

        // A brand-new file (Created-event shape) adds the first caller.
        let stats = apply_edit(
            &conn,
            "src/new.rs",
            &spec(
                vec![("fresh", "function")],
                vec![("existing", Some("fresh"), 0.9)],
                vec![],
            ),
        );
        assert_eq!(stats.rows_written, 1);
        let existing_id = symbol_id(&conn, "existing");
        let fresh_id = symbol_id(&conn, "fresh");
        assert_eq!(reach_rows(&conn, existing_id), vec![(fresh_id, 1, 0.9)]);
        assert_table_equivalent_to_bfs(&conn);
        assert_incremental_equals_full_rebuild(&conn);
    }

    #[test]
    fn repair_survives_cycle_introduction() {
        let (_dir, conn) = make_db();
        apply_edit(
            &conn,
            "src/hub.rs",
            &spec(vec![("hub", "function")], vec![], vec![]),
        );
        apply_edit(
            &conn,
            "src/a.rs",
            &spec(
                vec![("a", "function")],
                vec![("hub", Some("a"), 0.9)],
                vec![],
            ),
        );
        apply_edit(
            &conn,
            "src/b.rs",
            &spec(
                vec![("b", "function")],
                vec![("hub", Some("b"), 0.9)],
                vec![],
            ),
        );
        build(&conn, 3, DEFAULT_MAX_TARGETS_PER_SOURCE);

        // Introduce mutual recursion a <-> b (both files edited).
        let edit_a = spec(
            vec![("a", "function")],
            vec![("b", Some("a"), 0.9), ("hub", Some("a"), 0.9)],
            vec![],
        );
        let edit_b = spec(
            vec![("b", "function")],
            vec![("a", Some("b"), 0.9), ("hub", Some("b"), 0.9)],
            vec![],
        );
        apply_edit(&conn, "src/a.rs", &edit_a);
        apply_edit(&conn, "src/b.rs", &edit_b);

        // Terminates (this line running is the assert), stays equivalent.
        assert_table_equivalent_to_bfs(&conn);
        assert_incremental_equals_full_rebuild(&conn);

        // Repeat the same edits: content-idempotent (the symbol rowids
        // shift because the two files alternate holding the max id, so the
        // equivalence-oracle — not raw row snapshots — is the assert), and
        // no duplicated (source, target) pairs ever appear.
        apply_edit(&conn, "src/a.rs", &edit_a);
        apply_edit(&conn, "src/b.rs", &edit_b);
        assert_table_equivalent_to_bfs(&conn);
        assert_incremental_equals_full_rebuild(&conn);
        let pairs: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM (SELECT source_id, target_id FROM reach \
                 GROUP BY source_id, target_id HAVING COUNT(*) > 1)",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(pairs, 0, "no duplicated (source, target) pairs");
    }

    #[test]
    fn repair_noops_on_never_built_and_stale_tables() {
        // Never built: the edit commits, nothing is captured or written.
        let (_dir, conn) = make_db();
        let stats = apply_edit(
            &conn,
            "src/a.rs",
            &spec(
                vec![("a", "function")],
                vec![("missing", Some("a"), 0.9)],
                vec![],
            ),
        );
        assert!(stats.skipped, "never-built table is not repairable");
        assert_eq!(reach_row_count(&conn), 0);
        assert!(built_depth(&conn).is_none());
        assert!(!is_stale(&conn));

        // Stale: the edit commits, the marker stays, and the repair adds
        // or removes nothing. Rows referencing the edited file's symbols
        // vanish via FK cascade regardless of staleness; the assert is
        // that rows elsewhere survive untouched.
        apply_edit(
            &conn,
            "src/b.rs",
            &spec(vec![("b", "function")], vec![("a", Some("b"), 0.9)], vec![]),
        );
        apply_edit(
            &conn,
            "src/z.rs",
            &spec(vec![("z", "function")], vec![], vec![]),
        );
        apply_edit(
            &conn,
            "src/y.rs",
            &spec(vec![("y", "function")], vec![("z", Some("y"), 0.9)], vec![]),
        );
        build(&conn, 3, DEFAULT_MAX_TARGETS_PER_SOURCE);
        let z_id = symbol_id(&conn, "z");
        let y_id = symbol_id(&conn, "y");
        assert_eq!(reach_rows(&conn, z_id), vec![(y_id, 1, 0.9)]);

        let tx = conn.unchecked_transaction().unwrap();
        mark_stale(&tx).unwrap();
        tx.commit().unwrap();

        let stats = apply_edit(
            &conn,
            "src/a.rs",
            &spec(
                vec![("a2", "function")],
                vec![("b", Some("a2"), 0.9)],
                vec![],
            ),
        );
        assert!(stats.skipped, "stale table is not incrementally repairable");
        assert!(is_stale(&conn), "only a full rebuild clears the marker");
        assert_eq!(
            reach_rows(&conn, z_id),
            vec![(y_id, 1, 0.9)],
            "rows outside the edit survive untouched"
        );
        assert!(
            lookup_upstream(&conn, "b", 3).unwrap().is_none(),
            "stale table must fall back to BFS"
        );
    }

    #[test]
    fn repair_respects_built_depth() {
        let (_dir, conn) = make_db();
        // e1 <- e2 <- e3 <- e4, one file per link.
        apply_edit(
            &conn,
            "src/e1.rs",
            &spec(vec![("e1", "function")], vec![], vec![]),
        );
        apply_edit(
            &conn,
            "src/e2.rs",
            &spec(
                vec![("e2", "function")],
                vec![("e1", Some("e2"), 0.9)],
                vec![],
            ),
        );
        apply_edit(
            &conn,
            "src/e3.rs",
            &spec(
                vec![("e3", "function")],
                vec![("e2", Some("e3"), 0.9)],
                vec![],
            ),
        );
        apply_edit(
            &conn,
            "src/e4.rs",
            &spec(
                vec![("e4", "function")],
                vec![("e3", Some("e4"), 0.9)],
                vec![],
            ),
        );
        build(&conn, 2, DEFAULT_MAX_TARGETS_PER_SOURCE);

        // New depth-5 caller of e1: f5 -> e4 -> e3 -> e2 -> e1.
        apply_edit(
            &conn,
            "src/new.rs",
            &spec(
                vec![("f5", "function")],
                vec![("e4", Some("f5"), 0.9)],
                vec![],
            ),
        );

        assert_eq!(built_depth(&conn).as_deref(), Some("2"), "depth intact");
        let e1_rows = reach_rows(&conn, symbol_id(&conn, "e1"));
        assert!(
            e1_rows.iter().all(|&(_, depth, _)| depth <= 2),
            "repaired rows stay within built_depth: {e1_rows:?}"
        );
        // The chain runs f5 -> e4 -> e3 -> e2 -> e1: f5 sits at depth 5
        // from e1 (absent at built depth 2), depth 1 from e4, depth 2 from
        // e3, and depth 3 from e2 (absent).
        let e4_id = symbol_id(&conn, "e4");
        let e3_id = symbol_id(&conn, "e3");
        let e2_id = symbol_id(&conn, "e2");
        let f5_id = symbol_id(&conn, "f5");
        assert_eq!(
            reach_rows(&conn, e4_id),
            vec![(f5_id, 1, 0.9)],
            "f5 is a direct caller of e4"
        );
        assert_eq!(
            reach_rows(&conn, e3_id),
            vec![(e4_id, 1, 0.9), (f5_id, 2, 0.9)],
            "e4 calls e3, f5 two hops out"
        );
        assert_eq!(
            reach_rows(&conn, e2_id),
            vec![(e3_id, 1, 0.9), (e4_id, 2, 0.9)],
            "f5 at depth 3 from e2 is beyond built_depth"
        );
        assert_table_equivalent_to_bfs(&conn);
        assert_incremental_equals_full_rebuild(&conn);
    }

    #[test]
    fn repair_applies_cap_to_rebuilt_sources() {
        let (_dir, conn) = make_db();
        apply_edit(
            &conn,
            "src/hub.rs",
            &spec(vec![("hub", "function")], vec![], vec![]),
        );
        for f in 0..6 {
            let mut symbols: Vec<(String, String)> = Vec::new();
            let mut refs: Vec<(String, Option<String>, f64)> = Vec::new();
            for j in 0..100 {
                let name = format!("c{f}_{j}");
                symbols.push((name.clone(), "function".to_string()));
                refs.push(("hub".to_string(), Some(name), 0.9));
            }
            apply_edit(
                &conn,
                &format!("src/callers{f}.rs"),
                &FileSpec {
                    symbols,
                    refs,
                    type_edges: vec![],
                },
            );
        }
        // Built UNCAPPED: the hub records all 600 callers.
        build(&conn, 3, usize::MAX);
        let hub_id = symbol_id(&conn, "hub");
        let uncapped = reach_rows(&conn, hub_id);
        assert_eq!(uncapped.len(), 600);

        // Edit the hub's own file: the repair rewrites the hub's source
        // under the DEFAULT cap (the table self-describes; no config). The
        // edit replaces the hub symbol row, so the source id moves — read
        // it back after the edit.
        apply_edit(
            &conn,
            "src/hub.rs",
            &spec(
                vec![("hub", "function"), ("local", "function")],
                vec![("hub", Some("local"), 0.9)],
                vec![],
            ),
        );
        let hub_id = symbol_id(&conn, "hub");
        let capped = reach_rows(&conn, hub_id);
        assert_eq!(
            capped.len(),
            DEFAULT_MAX_TARGETS_PER_SOURCE,
            "repair applies the default fan-out cap"
        );
        let marked: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM reach_truncated WHERE source_id = ?1",
                rusqlite::params![hub_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(marked, 1, "capped source carries the truncation marker");
        // The capped set is a deterministic prefix of the uncapped set.
        for row in &capped {
            assert!(uncapped.contains(row), "capped row {row:?} not in full set");
        }
    }

    #[test]
    fn incremental_equivalence_random_edit_sequences() {
        for seed in 1..=10u64 {
            let (_dir, conn) = make_db();
            let mut rng = SplitMix64(seed);

            // Random initial state over all five fixture files.
            for file in EQUIV_FILES {
                apply_edit(&conn, file, &random_spec(&mut rng));
            }
            build(&conn, 3, DEFAULT_MAX_TARGETS_PER_SOURCE);
            assert_table_equivalent_to_bfs(&conn);

            for step in 0..5 {
                let file = EQUIV_FILES[rng.below(EQUIV_FILES.len())];
                apply_edit(&conn, file, &random_spec(&mut rng));

                let where_ = format!("seed {seed} step {step} file {file}");
                assert!(
                    !is_stale(&conn),
                    "{where_}: repair must keep the table fresh"
                );
                assert_eq!(
                    built_depth(&conn).as_deref(),
                    Some("3"),
                    "{where_}: built_depth must survive repairs"
                );
                assert_no_orphan_rows(&conn);
                assert_incremental_equals_full_rebuild(&conn);
            }

            // Once per seed: the full lookup-vs-BFS sweep.
            assert_table_equivalent_to_bfs(&conn);
        }
    }

    /// One random file body: 1..=6 symbols with colliding names and kinds,
    /// 0..=3 refs per symbol into the shared name pools (some unresolved),
    /// 0..=2 type edges with in-file children and in-file or cross-file
    /// parents.
    fn random_spec(rng: &mut SplitMix64) -> FileSpec {
        let symbol_count = 1 + rng.below(6);
        let mut symbols: Vec<(String, String)> = Vec::new();
        for _ in 0..symbol_count {
            let name = EQUIV_POOL[rng.below(EQUIV_POOL.len())];
            let kind = EQUIV_KINDS[rng.below(EQUIV_KINDS.len())];
            symbols.push((name.to_string(), kind.to_string()));
        }
        let mut refs: Vec<(String, Option<String>, f64)> = Vec::new();
        for (name, _) in &symbols {
            for _ in 0..rng.below(4) {
                let callee = if rng.below(3) == 0 {
                    EQUIV_STRUCTURED[rng.below(EQUIV_STRUCTURED.len())]
                } else {
                    EQUIV_POOL[rng.below(EQUIV_POOL.len())]
                };
                let caller = if rng.below(10) == 0 {
                    None
                } else {
                    Some(name.clone())
                };
                let confidence = EQUIV_CONFS[rng.below(EQUIV_CONFS.len())];
                refs.push((callee.to_string(), caller, confidence));
            }
        }
        let mut type_edges: Vec<(String, String)> = Vec::new();
        for _ in 0..rng.below(3) {
            let parent = EQUIV_POOL[rng.below(EQUIV_POOL.len())];
            let child = EQUIV_POOL[rng.below(EQUIV_POOL.len())];
            type_edges.push((parent.to_string(), child.to_string()));
        }
        FileSpec {
            symbols,
            refs,
            type_edges,
        }
    }

    // -- Concurrency suite (PRD-REACH-REQ-008, AR-028) -----------------------
    //
    // Readers must never observe a shrunken or partially-published reach set
    // while a writer re-indexes or rebuilds. WAL + one transaction per
    // publication give that; these tests hold the line.

    /// Variant A — daemon re-index loop: the writer drives the per-file
    /// upsert path (delete + reinsert + incremental reach repair in one
    /// transaction) while a reader repeatedly answers from a single read
    /// snapshot. The reader must always see the full expected set via BFS,
    /// and any table answer must be that same full set — never empty, never
    /// a subset.
    #[test]
    fn concurrency_reader_never_observes_shrunken_set_during_reindex() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().to_path_buf();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::create_dir(root.join(".git")).unwrap();
        let lib = root.join("src/lib.rs");
        let base = "fn hello() { world(); }\nfn world() { 42 }\n";
        fs::write(&lib, base).unwrap();

        crate::pipeline::build_index(&root, true).unwrap();
        let index_path = db::local_index_path(&root);

        // The toggle comment shifts line numbers, so the stable expectation
        // is the affected NAME SET, not exact locations.
        let expected_names: Vec<String> = {
            let conn = db::open_existing(&index_path).unwrap();
            let analysis =
                crate::blast::analyze_blast(&conn, "world", &bfs_options(3, false)).unwrap();
            assert_eq!(
                analysis.total_affected, 1,
                "fixture sanity: exactly hello calls world"
            );
            analysis
                .tiers
                .iter()
                .flat_map(|t| t.symbols.iter().map(|s| s.name.clone()))
                .collect()
        };
        assert_eq!(expected_names, vec!["hello".to_string()]);

        let done = Arc::new(AtomicBool::new(false));
        let observations = Arc::new(AtomicUsize::new(0));

        let writer_root = root.clone();
        let writer = std::thread::spawn(move || {
            let wconn = db::open_existing(&db::local_index_path(&writer_root)).unwrap();
            for i in 0..30 {
                let content = if i % 2 == 0 {
                    format!("// toggle {i}\n{base}")
                } else {
                    base.to_string()
                };
                fs::write(&lib, content).unwrap();
                crate::pipeline::reindex_file(&wconn, &lib, &writer_root).unwrap();
            }
        });

        let reader_root = root.clone();
        let reader_done = Arc::clone(&done);
        let reader_observations = Arc::clone(&observations);
        let reader = std::thread::spawn(move || {
            let rconn = db::open_existing(&db::local_index_path(&reader_root)).unwrap();
            while !reader_done.load(Ordering::Relaxed) {
                let tx = rconn.unchecked_transaction().unwrap();
                let bfs =
                    crate::blast::analyze_blast(&tx, "world", &bfs_options(3, false)).unwrap();
                let observed: Vec<String> = bfs
                    .tiers
                    .iter()
                    .flat_map(|t| t.symbols.iter().map(|s| s.name.clone()))
                    .collect();
                assert_eq!(
                    observed, expected_names,
                    "reader must always observe the full set on its snapshot"
                );
                if let Some(answer) = lookup_upstream_impl(&tx, "world", 3).unwrap() {
                    let names: Vec<String> =
                        answer.affected.iter().map(|s| s.name.clone()).collect();
                    assert_eq!(
                        names, expected_names,
                        "a table answer is authoritative or absent, never a subset"
                    );
                    assert!(!answer.truncated);
                }
                tx.commit().unwrap();
                reader_observations.fetch_add(1, Ordering::Relaxed);
            }
        });

        writer.join().unwrap();
        done.store(true, Ordering::SeqCst);
        reader.join().unwrap();
        assert!(
            observations.load(Ordering::SeqCst) > 0,
            "reader must have shared the timeline with the writer"
        );
    }

    /// Variant B — full rebuild loop: the writer rebuilds the whole index
    /// while a reader answers per snapshot. A table answer must be the full
    /// expected set; the only window where the table may be absent is the
    /// one where the symbols themselves are gone (rules out partial
    /// publication of reach rows ahead of or behind the symbols).
    #[test]
    fn concurrency_reader_never_observes_partial_publication_during_rebuild() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().to_path_buf();
        fs::create_dir(root.join(".git")).unwrap();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::create_dir_all(root.join("tests")).unwrap();
        for (path, content) in [
            ("src/one.rs", "fn one() { two(); }\n"),
            ("src/two.rs", "fn two() { three(); }\n"),
            ("src/three.rs", "fn three() { four(); }\n"),
            ("src/four.rs", "fn four() { }\n"),
            ("src/deep.rs", "fn deep_caller() { one(); }\n"),
            ("tests/chain_test.rs", "fn chain_suite() { two(); }\n"),
        ] {
            fs::write(root.join(path), content).unwrap();
        }

        crate::pipeline::build_index(&root, true).unwrap();
        let index_path = db::local_index_path(&root);

        let expected = {
            let conn = db::open_existing(&index_path).unwrap();
            crate::blast::analyze_blast(&conn, "two", &bfs_options(3, false)).unwrap()
        };
        assert!(
            expected.total_affected >= 2,
            "fixture sanity: two has callers (one, deep_caller)"
        );

        let done = Arc::new(AtomicBool::new(false));
        let observations = Arc::new(AtomicUsize::new(0));

        let writer_root = root.clone();
        let writer = std::thread::spawn(move || {
            for _ in 0..10 {
                crate::pipeline::build_index(&writer_root, true).unwrap();
            }
        });

        let reader_root = root.clone();
        let reader_done = Arc::clone(&done);
        let reader_observations = Arc::clone(&observations);
        let reader = std::thread::spawn(move || {
            let rconn = db::open_existing(&db::local_index_path(&reader_root)).unwrap();
            while !reader_done.load(Ordering::Relaxed) {
                let tx = rconn.unchecked_transaction().unwrap();
                let symbols: i64 = tx
                    .query_row("SELECT COUNT(*) FROM symbols", [], |row| row.get(0))
                    .unwrap();
                if let Some(answer) = lookup_upstream_impl(&tx, "two", 3).unwrap() {
                    assert!(symbols > 0, "an answer implies a populated snapshot");
                    assert_eq!(
                        answer.affected.len(),
                        expected.total_affected,
                        "table answers are the full set, never partially published"
                    );
                    let bfs =
                        crate::blast::analyze_blast(&tx, "two", &bfs_options(3, false)).unwrap();
                    assert_eq!(bfs, expected, "table ≡ BFS on the same snapshot");
                } else {
                    assert_eq!(
                        symbols, 0,
                        "absent table is allowed only inside the empty drop window"
                    );
                }
                tx.commit().unwrap();
                reader_observations.fetch_add(1, Ordering::Relaxed);
            }
        });

        writer.join().unwrap();
        done.store(true, Ordering::SeqCst);
        reader.join().unwrap();
        assert!(
            observations.load(Ordering::SeqCst) > 0,
            "reader must have shared the timeline with the writer"
        );
    }
}
