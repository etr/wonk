//! Graph-topology scoring (TASK-098, DR-040): hub and authority scores
//! over the reference call graph, recomputed as a DISTINCT pass on a
//! cadence — never as part of per-file re-indexing (PRD-TOPO-REQ-006),
//! because the scores are global graph properties a file edit cannot
//! update locally.
//!
//! Edges are `references.caller_id` → the callee NAME resolved to the
//! canonical `MIN(id)` non-module symbol (the reach.rs pattern): the
//! name-collated representative, not `target_id`, whose cross-file
//! resolution is partial and would silently drop ambiguous edges. HITS
//! runs the power method for EXACTLY `iterations` iterations
//! (PRD-TOPO-REQ-005) — a fixed count is the bound and the determinism
//! guarantee, since the id-ascending node order, the MIN(id)
//! representatives, and the sorted CSR iteration fix the f64 operation
//! sequence. Stale scores are always served, never recomputed on the
//! query path (PRD-TOPO-REQ-007); `topology_meta.last_computed` drives
//! the cadence gate and the staleness marker.
//!
//! TASK-099 adds connectivity communities over the same graph
//! (PRD-TOPO-REQ-002): deterministic label propagation whose community
//! id is the MIN(symbol_id) of the group's members — stable across runs
//! by construction. Distinct from `wonk cluster`, which groups by
//! embedding similarity; a community groups symbols that CALL each
//! other, across directories.

use anyhow::Result;
use rusqlite::Connection;
use std::collections::{HashMap, HashSet};
use std::time::{SystemTime, UNIX_EPOCH};

/// `topology_meta` key holding the epoch seconds of the last successful
/// recompute — the cadence gate's clock and the staleness marker's
/// reference point (PRD-TOPO-REQ-007).
pub const META_LAST_COMPUTED: &str = "last_computed";

/// The recompute knobs a caller passes through `recompute`, carried as
/// one struct so adding a knob never touches the call sites again (the
/// `MiningOptions` pattern; built from `[topology]` via
/// `From<&TopologyConfig>`).
#[derive(Debug, Clone, Copy)]
pub struct TopologyOptions {
    /// Exact number of power-method iterations — the bound itself
    /// (PRD-TOPO-REQ-005), not a convergence tolerance.
    pub iterations: usize,
    /// Sweep cap for label-propagation community detection — a bound,
    /// not a target: a converged graph stops early.
    pub community_passes: usize,
}

impl From<&crate::config::TopologyConfig> for TopologyOptions {
    fn from(config: &crate::config::TopologyConfig) -> Self {
        Self {
            iterations: config.iterations,
            community_passes: config.community_passes,
        }
    }
}

/// The call graph in CSR form: one flat adjacency array plus `n + 1`
/// offsets per direction, so the HITS inner loops walk contiguous memory
/// in a fixed (ascending) order — part of the determinism guarantee.
struct TopologyGraph {
    /// Dense node ids, position-ascending (Q1's ORDER BY id).
    ids: Vec<i64>,
    successors: Vec<usize>,
    succ_offsets: Vec<usize>,
    predecessors: Vec<usize>,
    pred_offsets: Vec<usize>,
}

impl TopologyGraph {
    fn successors(&self, node: usize) -> &[usize] {
        &self.successors[self.succ_offsets[node]..self.succ_offsets[node + 1]]
    }

    fn predecessors(&self, node: usize) -> &[usize] {
        &self.predecessors[self.pred_offsets[node]..self.pred_offsets[node + 1]]
    }

    /// CSR arrays for `nodes`-many vertices over the SORTED `edges`
    /// `(src, dst)` pairs: counting sort by source, then one fill pass in
    /// edge order, which leaves every inner list ascending.
    fn csr(nodes: usize, edges: &[(usize, usize)], flipped: bool) -> (Vec<usize>, Vec<usize>) {
        let mut offsets = vec![0usize; nodes + 1];
        for &(src, dst) in edges {
            let from = if flipped { dst } else { src };
            offsets[from + 1] += 1;
        }
        for i in 0..nodes {
            offsets[i + 1] += offsets[i];
        }
        let mut adjacency = vec![0usize; edges.len()];
        let mut cursor = offsets.clone();
        for &(src, dst) in edges {
            let (from, to) = if flipped { (dst, src) } else { (src, dst) };
            adjacency[cursor[from]] = to;
            cursor[from] += 1;
        }
        (adjacency, offsets)
    }
}

/// Load the call graph (PRD-TOPO-REQ-001).
///
/// Three queries, all deterministic in shape: (1) every symbol id in
/// ascending order — THE node order, which fixes array positions; (2)
/// the canonical `MIN(id)` non-module representative per name — the
/// callee resolution, with modules excluded so import references cannot
/// accumulate authority; (3) every caller-annotated reference. Edges
/// whose callee name has no non-module representative drop out, and
/// duplicate (caller, callee) pairs collapse in the dedup set — the
/// graph is unweighted, so a symbol referencing a callee twice is still
/// one edge.
fn load_graph(conn: &Connection) -> Result<TopologyGraph> {
    let mut ids = Vec::new();
    {
        let mut stmt = conn.prepare("SELECT id FROM symbols ORDER BY id")?;
        let rows = stmt.query_map([], |row| row.get::<_, i64>(0))?;
        for row in rows {
            ids.push(row?);
        }
    }
    let pos: HashMap<i64, usize> = ids.iter().enumerate().map(|(p, &id)| (id, p)).collect();

    let mut representatives: HashMap<String, i64> = HashMap::new();
    {
        let mut stmt = conn.prepare(
            "SELECT name, MIN(id) FROM symbols \
             WHERE kind <> 'module' GROUP BY name",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?;
        for row in rows {
            let (name, id) = row?;
            representatives.insert(name, id);
        }
    }

    let mut edge_set: HashSet<(usize, usize)> = HashSet::new();
    {
        let mut stmt =
            conn.prepare("SELECT caller_id, name FROM \"references\" WHERE caller_id IS NOT NULL")?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })?;
        for row in rows {
            let (caller_id, callee_name) = row?;
            let Some(&caller_pos) = pos.get(&caller_id) else {
                continue;
            };
            let Some(&callee_id) = representatives.get(&callee_name) else {
                continue;
            };
            let Some(&callee_pos) = pos.get(&callee_id) else {
                continue;
            };
            edge_set.insert((caller_pos, callee_pos));
        }
    }

    let mut edges: Vec<(usize, usize)> = edge_set.into_iter().collect();
    edges.sort_unstable();
    let n = ids.len();
    let (successors, succ_offsets) = TopologyGraph::csr(n, &edges, false);
    let (predecessors, pred_offsets) = TopologyGraph::csr(n, &edges, true);
    Ok(TopologyGraph {
        ids,
        successors,
        succ_offsets,
        predecessors,
        pred_offsets,
    })
}

/// The HITS power method over the CSR graph, for EXACTLY `iterations`
/// iterations with per-iteration L1 normalization (PRD-TOPO-REQ-005).
///
/// Starting from the all-ones hub vector, each iteration computes
/// `auth = Aᵀ·hub` then `hub = A·auth`, normalizing each to unit L1 as
/// it lands. A zero L1 sum (an edgeless graph, reached on the first
/// iteration) zeroes both vectors and breaks deterministically — there
/// is no epsilon convergence loop, so the f64 operation sequence is a
/// pure function of the graph shape.
fn hits(graph: &TopologyGraph, iterations: usize) -> (Vec<f64>, Vec<f64>) {
    let n = graph.ids.len();
    let mut hub = vec![1.0f64; n];
    let mut auth = vec![0.0f64; n];
    for _ in 0..iterations {
        let mut auth_next = vec![0.0f64; n];
        for (v, slot) in auth_next.iter_mut().enumerate() {
            *slot = graph.predecessors(v).iter().map(|&u| hub[u]).sum();
        }
        let auth_sum: f64 = auth_next.iter().sum();
        if auth_sum == 0.0 {
            hub.iter_mut().for_each(|h| *h = 0.0);
            auth.iter_mut().for_each(|a| *a = 0.0);
            break;
        }
        for a in &mut auth_next {
            *a /= auth_sum;
        }
        auth = auth_next;

        let mut hub_next = vec![0.0f64; n];
        for (u, slot) in hub_next.iter_mut().enumerate() {
            *slot = graph.successors(u).iter().map(|&v| auth[v]).sum();
        }
        let hub_sum: f64 = hub_next.iter().sum();
        if hub_sum == 0.0 {
            hub.iter_mut().for_each(|h| *h = 0.0);
            auth.iter_mut().for_each(|a| *a = 0.0);
            break;
        }
        for h in &mut hub_next {
            *h /= hub_sum;
        }
        hub = hub_next;
    }
    (hub, auth)
}

/// Deterministic asynchronous label propagation over the UNDIRECTED
/// view of the call graph (TASK-099, PRD-TOPO-REQ-002): a caller and
/// its callee belong to the same subsystem regardless of edge
/// direction, so both CSR directions vote. Returns the community id —
/// a symbol id — per CSR node position.
///
/// Each sweep visits positions in ascending order (the deterministic
/// sweep order); every node adopts the majority label among its
/// neighbors, ties breaking to the SMALLEST label, so the comparison
/// is a total order and a `HashMap` tally stays deterministic
/// (PRD-TOPO-REQ-005). Self-loops do not vote — recursion must not
/// lock a node's own label — and a node with no voting neighbor keeps
/// its label. Sweeps stop early once one changes nothing, so `passes`
/// caps the work without changing a converged result. Final labels are
/// canonicalized to the smallest member position of each group:
/// positions are id-ascending, so a community id IS the MIN(symbol_id)
/// of its members — inherently stable across runs on an unchanged
/// graph, and integer-only, so "stable" means exactly equal.
fn detect_communities(graph: &TopologyGraph, passes: usize) -> Vec<i64> {
    let n = graph.ids.len();
    let mut labels: Vec<usize> = (0..n).collect();
    for _ in 0..passes {
        let mut changed = false;
        for v in 0..n {
            let mut tally: HashMap<usize, usize> = HashMap::new();
            for &u in graph.successors(v).iter().chain(graph.predecessors(v)) {
                if u == v {
                    continue;
                }
                *tally.entry(labels[u]).or_insert(0) += 1;
            }
            let best = tally
                .into_iter()
                .max_by(|&(label_a, count_a), &(label_b, count_b)| {
                    count_a.cmp(&count_b).then(label_b.cmp(&label_a))
                });
            if let Some((best, _)) = best
                && best != labels[v]
            {
                labels[v] = best;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }

    let mut first_of_label: HashMap<usize, i64> = HashMap::new();
    labels
        .iter()
        .enumerate()
        .map(|(v, &label)| *first_of_label.entry(label).or_insert(graph.ids[v]))
        .collect()
}

/// Recompute hub and authority scores for the whole graph, replacing any
/// previous rows and stamping `last_computed` — the full-build and
/// `wonk update` path, run unconditionally when enabled.
pub fn recompute(conn: &Connection, opts: &TopologyOptions) -> Result<()> {
    let graph = load_graph(conn)?;
    let (hub, auth) = hits(&graph, opts.iterations);
    let community = detect_communities(&graph, opts.community_passes);

    // One transaction: the scores and their stamp land together, so a
    // reader never sees fresh rows with an old (or missing) marker.
    let tx = conn.unchecked_transaction()?;
    tx.execute("DELETE FROM symbol_topology", [])?;
    {
        // Rows land position-ascending = id-ascending: the deterministic
        // write order. Community is written for EVERY symbol row —
        // singletons carry their own id, never NULL.
        const ROWS_PER_STMT: usize = 400;
        let rows: Vec<(i64, f64, f64, i64)> = graph
            .ids
            .iter()
            .enumerate()
            .map(|(p, &id)| (id, hub[p], auth[p], community[p]))
            .collect();
        for chunk in rows.chunks(ROWS_PER_STMT) {
            let placeholders = chunk
                .iter()
                .map(|_| "(?, ?, ?, ?)")
                .collect::<Vec<_>>()
                .join(", ");
            let sql = format!(
                "INSERT INTO symbol_topology (symbol_id, hub, authority, community) \
                 VALUES {placeholders}"
            );
            let params: Vec<&dyn rusqlite::ToSql> = chunk
                .iter()
                .flat_map(|(id, h, a, c)| {
                    [id as &dyn rusqlite::ToSql, h, a, c as &dyn rusqlite::ToSql]
                })
                .collect();
            tx.execute(&sql, params.as_slice())?;
        }
    }
    tx.execute(
        "INSERT OR REPLACE INTO topology_meta (key, value) VALUES (?1, ?2)",
        rusqlite::params![META_LAST_COMPUTED, epoch_now().to_string()],
    )?;
    tx.commit()?;
    Ok(())
}

/// Recompute only when the cadence gate is open (the daemon hook,
/// PRD-TOPO-REQ-006): no `last_computed` yet, or at least
/// `interval_secs` have elapsed since it. Returns whether a recompute
/// ran.
pub fn refresh_if_due(
    conn: &Connection,
    opts: &TopologyOptions,
    interval_secs: u64,
) -> Result<bool> {
    let due = match last_computed(conn) {
        None => true,
        Some(stamp) => epoch_now().saturating_sub(stamp) >= interval_secs as i64,
    };
    if !due {
        return Ok(false);
    }
    recompute(conn, opts)?;
    Ok(true)
}

/// Epoch seconds of the last successful recompute; `None` when the pass
/// has never run (a pre-TASK-098 index, or topology disabled).
pub fn last_computed(conn: &Connection) -> Option<i64> {
    conn.query_row(
        "SELECT value FROM topology_meta WHERE key = ?1",
        rusqlite::params![META_LAST_COMPUTED],
        |row| row.get::<_, String>(0),
    )
    .ok()
    .and_then(|value| value.parse().ok())
}

/// Whether the stored scores are older than `stale_after_secs`
/// (PRD-TOPO-REQ-007). Never a query-path concern: stale scores are
/// served with a marker, never awaited on. A pass that never ran is
/// absent data, not stale data — the status line separates the two.
pub fn is_stale(conn: &Connection, stale_after_secs: u64) -> bool {
    match last_computed(conn) {
        None => false,
        Some(stamp) => epoch_now().saturating_sub(stamp) > stale_after_secs as i64,
    }
}

/// Current unix time in whole seconds; 0 if the clock is before the
/// epoch (unreachable in practice, but a deterministic fallback).
fn epoch_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn db() -> (TempDir, Connection) {
        let dir = TempDir::new().unwrap();
        let conn = crate::db::open(&dir.path().join("index.db")).unwrap();
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

    fn insert_ref(conn: &Connection, callee: &str, caller_id: Option<i64>) {
        conn.execute(
            "INSERT INTO \"references\" (name, file, line, col, caller_id) \
             VALUES (?1, 'src/lib.rs', 1, 1, ?2)",
            rusqlite::params![callee, caller_id],
        )
        .unwrap();
    }

    /// (symbol_id, hub bits, authority bits) — the bit patterns, so
    /// equality asserts are BITWISE, not approximate.
    fn topology_bits(conn: &Connection) -> Vec<(i64, u64, u64)> {
        conn.prepare("SELECT symbol_id, hub, authority FROM symbol_topology ORDER BY symbol_id")
            .unwrap()
            .query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, f64>(1)?.to_bits(),
                    row.get::<_, f64>(2)?.to_bits(),
                ))
            })
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    }

    fn score(conn: &Connection, name: &str) -> (f64, f64) {
        conn.query_row(
            "SELECT t.hub, t.authority FROM symbol_topology t \
             JOIN symbols s ON s.id = t.symbol_id WHERE s.name = ?1",
            rusqlite::params![name],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap()
    }

    /// Stored community of a symbol by name; `None` until TASK-099's
    /// recompute writes the column.
    fn community(conn: &Connection, name: &str) -> Option<i64> {
        conn.query_row(
            "SELECT t.community FROM symbol_topology t \
             JOIN symbols s ON s.id = t.symbol_id WHERE s.name = ?1",
            rusqlite::params![name],
            |row| row.get(0),
        )
        .unwrap()
    }

    fn symbol_id(conn: &Connection, name: &str) -> i64 {
        conn.query_row(
            "SELECT id FROM symbols WHERE name = ?1",
            rusqlite::params![name],
            |row| row.get(0),
        )
        .unwrap()
    }

    /// The full (symbol_id, community) assignment, id-ascending —
    /// integers, so equality asserts are exact.
    fn communities(conn: &Connection) -> Vec<(i64, i64)> {
        conn.prepare("SELECT symbol_id, community FROM symbol_topology ORDER BY symbol_id")
            .unwrap()
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    }

    fn opts(iterations: usize, community_passes: usize) -> TopologyOptions {
        TopologyOptions {
            iterations,
            community_passes,
        }
    }

    /// The core-vs-leaf fixture: `core` is depended on by five callers
    /// plus `orchestrator`; `leaf` is called only twice; `orchestrator`
    /// calls two symbols while `single_caller` calls one.
    fn seed_core_vs_leaf(conn: &Connection) {
        insert_symbol(conn, "core", "class", "src/core.rs", 1);
        insert_symbol(conn, "leaf", "function", "src/leaf.rs", 1);
        let orchestrator = insert_symbol(conn, "orchestrator", "function", "src/orch.rs", 1);
        let single_caller = insert_symbol(conn, "single_caller", "function", "src/single.rs", 1);
        for i in 0..5 {
            let caller = insert_symbol(conn, &format!("c{i}"), "function", "src/c{i}.rs", 1);
            insert_ref(conn, "core", Some(caller));
        }
        insert_ref(conn, "core", Some(orchestrator));
        insert_ref(conn, "leaf", Some(orchestrator));
        insert_ref(conn, "leaf", Some(single_caller));
    }

    #[test]
    fn widely_depended_upon_core_type_outranks_leaf_on_authority() {
        let (_dir, conn) = db();
        seed_core_vs_leaf(&conn);

        recompute(&conn, &opts(20, 30)).unwrap();

        let (_, core_auth) = score(&conn, "core");
        let (_, leaf_auth) = score(&conn, "leaf");
        assert!(
            core_auth > leaf_auth,
            "the core type's authority must outrank the uncalled leaf's: {core_auth} vs {leaf_auth}"
        );
        let (orch_hub, _) = score(&conn, "orchestrator");
        let (single_hub, _) = score(&conn, "single_caller");
        assert!(
            orch_hub > single_hub,
            "a caller of the authoritative core must outrank a leaf-only caller on hub: \
             {orch_hub} vs {single_hub}"
        );
    }

    #[test]
    fn scores_are_bitwise_identical_across_runs_connections_and_insert_orders() {
        let (_dir_a, conn_a) = db();
        seed_core_vs_leaf(&conn_a);
        recompute(&conn_a, &opts(20, 30)).unwrap();
        let first = topology_bits(&conn_a);

        // Same connection, recomputed.
        recompute(&conn_a, &opts(20, 30)).unwrap();
        assert_eq!(
            topology_bits(&conn_a),
            first,
            "recompute is a pure function"
        );

        // A second database with the same graph.
        let (_dir_b, conn_b) = db();
        seed_core_vs_leaf(&conn_b);
        recompute(&conn_b, &opts(20, 30)).unwrap();
        assert_eq!(
            topology_bits(&conn_b),
            first,
            "identical graph, identical bits"
        );

        // The same graph with references inserted in the opposite order.
        let (_dir_c, conn_c) = db();
        insert_symbol(&conn_c, "core", "class", "src/core.rs", 1);
        insert_symbol(&conn_c, "leaf", "function", "src/leaf.rs", 1);
        let orchestrator = insert_symbol(&conn_c, "orchestrator", "function", "src/orch.rs", 1);
        let single_caller = insert_symbol(&conn_c, "single_caller", "function", "src/single.rs", 1);
        let mut callers = Vec::new();
        for i in 0..5 {
            callers.push(insert_symbol(
                &conn_c,
                &format!("c{i}"),
                "function",
                "src/c{i}.rs",
                1,
            ));
        }
        for caller in callers.iter().rev() {
            insert_ref(&conn_c, "core", Some(*caller));
        }
        insert_ref(&conn_c, "leaf", Some(single_caller));
        insert_ref(&conn_c, "leaf", Some(orchestrator));
        insert_ref(&conn_c, "core", Some(orchestrator));
        recompute(&conn_c, &opts(20, 30)).unwrap();
        assert_eq!(
            topology_bits(&conn_c),
            first,
            "insertion order must not move a single bit"
        );
    }

    #[test]
    fn one_iteration_is_normalized_indegree_and_counts_change_scores() {
        let (_dir, conn) = db();
        seed_core_vs_leaf(&conn);

        recompute(&conn, &opts(1, 30)).unwrap();

        // After exactly one iteration authority is the L1-normalized
        // in-degree: core has 6 in-edges, leaf 2, of 8 total.
        let (_, core_auth) = score(&conn, "core");
        let (_, leaf_auth) = score(&conn, "leaf");
        assert!((core_auth - 6.0 / 8.0).abs() < 1e-12, "got {core_auth}");
        assert!((leaf_auth - 2.0 / 8.0).abs() < 1e-12, "got {leaf_auth}");

        // A graph whose hub/authority coupling keeps evolving under the
        // power method (three callers into s2, which alone calls s3 —
        // authority mass concentrates geometrically, ratio ~1/3 per
        // iteration): 3 and 10 iterations must differ in the bits.
        let (_dir2, conn3) = db();
        let s1 = insert_symbol(&conn3, "s1", "function", "src/a.rs", 1);
        let s2 = insert_symbol(&conn3, "s2", "function", "src/a.rs", 2);
        insert_symbol(&conn3, "s3", "function", "src/a.rs", 3);
        let s4 = insert_symbol(&conn3, "s4", "function", "src/a.rs", 4);
        let s5 = insert_symbol(&conn3, "s5", "function", "src/a.rs", 5);
        for caller in [s1, s4, s5] {
            insert_ref(&conn3, "s2", Some(caller));
        }
        insert_ref(&conn3, "s3", Some(s2));
        recompute(&conn3, &opts(3, 30)).unwrap();
        let at_3 = topology_bits(&conn3);
        recompute(&conn3, &opts(10, 30)).unwrap();
        let at_10 = topology_bits(&conn3);
        assert_ne!(at_3, at_10, "the iteration count is a real bound");
    }

    #[test]
    fn last_computed_recorded_even_for_an_empty_graph() {
        let (_dir, conn) = db();
        recompute(&conn, &opts(20, 30)).unwrap();

        assert!(last_computed(&conn).is_some());
        let rows: i64 = conn
            .query_row("SELECT COUNT(*) FROM symbol_topology", [], |row| row.get(0))
            .unwrap();
        assert_eq!(rows, 0);
        // No row ever lands without a community after a recompute.
        let nulls: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM symbol_topology WHERE community IS NULL",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(nulls, 0);
    }

    #[test]
    fn null_caller_and_module_only_callee_edges_are_dropped() {
        let (_dir, conn) = db();
        insert_symbol(&conn, "mod_sym", "module", "src/mod.rs", 1);
        let caller = insert_symbol(&conn, "caller_fn", "function", "src/a.rs", 1);

        // A module-only callee name must not resolve (import references
        // must not accumulate authority); a NULL caller has no edge.
        insert_ref(&conn, "mod_sym", Some(caller));
        insert_ref(&conn, "caller_fn", None);

        recompute(&conn, &opts(20, 30)).unwrap();

        // Edgeless graph: every symbol still scored, all-zero, and the
        // deterministic break leaves the initial state flat.
        let rows: i64 = conn
            .query_row("SELECT COUNT(*) FROM symbol_topology", [], |row| row.get(0))
            .unwrap();
        assert_eq!(rows, 2, "every symbol gets a row");
        for (hub, authority) in [score(&conn, "mod_sym"), score(&conn, "caller_fn")] {
            assert_eq!(hub, 0.0);
            assert_eq!(authority, 0.0);
        }
        // Edgeless symbols are their own singleton community — never NULL.
        assert_eq!(
            community(&conn, "mod_sym"),
            Some(symbol_id(&conn, "mod_sym"))
        );
        assert_eq!(
            community(&conn, "caller_fn"),
            Some(symbol_id(&conn, "caller_fn"))
        );
    }

    #[test]
    fn duplicate_edges_collapse_to_one() {
        // Two references on the same (caller, callee) pair are ONE edge:
        // the graph is unweighted, so authority must not double.
        let (_dir, conn) = db();
        insert_symbol(&conn, "callee", "function", "src/a.rs", 1);
        let caller = insert_symbol(&conn, "caller", "function", "src/b.rs", 1);
        insert_ref(&conn, "callee", Some(caller));
        insert_ref(&conn, "callee", Some(caller));
        recompute(&conn, &opts(1, 30)).unwrap();
        let doubled = score(&conn, "callee");

        let (_dir2, conn2) = db();
        insert_symbol(&conn2, "callee", "function", "src/a.rs", 1);
        let caller2 = insert_symbol(&conn2, "caller", "function", "src/b.rs", 1);
        insert_ref(&conn2, "callee", Some(caller2));
        recompute(&conn2, &opts(1, 30)).unwrap();
        let single = score(&conn2, "callee");

        assert_eq!(
            doubled, single,
            "multi-edges collapse: {doubled:?} vs {single:?}"
        );
    }

    #[test]
    fn refresh_if_due_is_gated_by_the_interval_since_last_computed() {
        let (_dir, conn) = db();

        // No meta yet: the first observation is due immediately.
        assert!(refresh_if_due(&conn, &opts(20, 30), 3600).unwrap());
        let stamp = last_computed(&conn).expect("recompute stamps the meta");

        // Fresh: within the interval, nothing runs and the stamp holds.
        assert!(!refresh_if_due(&conn, &opts(20, 30), 3600).unwrap());
        assert_eq!(last_computed(&conn), Some(stamp));

        // Aged past the interval: recompute and restamp.
        conn.execute(
            "UPDATE topology_meta SET value = ?1 WHERE key = ?2",
            rusqlite::params![(stamp - 7200).to_string(), META_LAST_COMPUTED],
        )
        .unwrap();
        assert!(refresh_if_due(&conn, &opts(20, 30), 3600).unwrap());
        let restamped = last_computed(&conn).expect("aged refresh restamps");
        assert!(restamped >= stamp);
    }

    #[test]
    fn recompute_replaces_prior_rows() {
        let (_dir, conn) = db();
        insert_symbol(&conn, "callee", "function", "src/a.rs", 1);
        let caller = insert_symbol(&conn, "caller", "function", "src/b.rs", 1);
        insert_ref(&conn, "callee", Some(caller));
        recompute(&conn, &opts(20, 30)).unwrap();
        let (_, authority_before) = score(&conn, "callee");
        assert!(authority_before > 0.0);

        // The edge disappears; the old scores must not survive it.
        conn.execute("DELETE FROM \"references\"", []).unwrap();
        recompute(&conn, &opts(20, 30)).unwrap();

        let (_, authority_after) = score(&conn, "callee");
        assert_eq!(authority_after, 0.0);
        let rows: i64 = conn
            .query_row("SELECT COUNT(*) FROM symbol_topology", [], |row| row.get(0))
            .unwrap();
        assert_eq!(rows, 2, "exactly one row per symbol, never leftovers");
    }

    #[test]
    fn is_stale_reads_the_age_against_the_threshold() {
        let (_dir, conn) = db();
        assert!(!is_stale(&conn, 60), "never computed is absent, not stale");
        recompute(&conn, &opts(20, 30)).unwrap();
        assert!(!is_stale(&conn, 86_400), "a fresh score is not stale");

        let stamp = last_computed(&conn).unwrap();
        conn.execute(
            "UPDATE topology_meta SET value = ?1 WHERE key = ?2",
            rusqlite::params![(stamp - 90_000).to_string(), META_LAST_COMPUTED],
        )
        .unwrap();
        assert!(is_stale(&conn, 86_400), "an aged score is stale");
    }

    // -- communities (TASK-099) -------------------------------------------------

    /// CSR position of a symbol id — positions are id-ascending, so this
    /// is a plain lookup.
    fn pos_of(graph: &TopologyGraph, id: i64) -> usize {
        graph.ids.iter().position(|&x| x == id).unwrap()
    }

    #[test]
    fn options_carry_the_community_pass_knob() {
        let options = TopologyOptions::from(&crate::config::TopologyConfig::default());
        assert_eq!(options.iterations, 20);
        assert_eq!(options.community_passes, 30);
    }

    #[test]
    fn community_id_is_the_min_symbol_id_of_its_group() {
        let (_dir, conn) = db();
        let b = insert_symbol(&conn, "b", "function", "src/b.rs", 1);
        let a = insert_symbol(&conn, "a", "function", "src/b.rs", 2);
        let lone = insert_symbol(&conn, "lone", "function", "src/b.rs", 3);
        insert_ref(&conn, "a", Some(b));
        insert_ref(&conn, "b", Some(a));

        let graph = load_graph(&conn).unwrap();
        let community = detect_communities(&graph, 30);

        // a and b call each other: one group, keyed by MIN(symbol_id) —
        // b was inserted first, so it carries the group's id.
        assert_eq!(community[pos_of(&graph, a)], b);
        assert_eq!(community[pos_of(&graph, b)], b);
        // An edgeless symbol is its own singleton community.
        assert_eq!(community[pos_of(&graph, lone)], lone);
    }

    #[test]
    fn two_cliques_split_into_two_communities() {
        let (_dir, conn) = db();
        let mut clique_x = Vec::new();
        let mut clique_y = Vec::new();
        for i in 0..5 {
            clique_x.push(insert_symbol(
                &conn,
                &format!("x{i}"),
                "function",
                "src/x.rs",
                i + 1,
            ));
            clique_y.push(insert_symbol(
                &conn,
                &format!("y{i}"),
                "function",
                "src/y.rs",
                i + 1,
            ));
        }
        for i in 0..5 {
            for j in 0..5 {
                if i != j {
                    insert_ref(&conn, &format!("x{j}"), Some(clique_x[i]));
                    insert_ref(&conn, &format!("y{j}"), Some(clique_y[i]));
                }
            }
        }
        // A single bridge between the LAST members — enough coupling to
        // observe, too little to merge the cliques.
        insert_ref(&conn, "y4", Some(clique_x[4]));

        let graph = load_graph(&conn).unwrap();
        let community = detect_communities(&graph, 30);

        let distinct: HashSet<i64> = community.iter().copied().collect();
        assert_eq!(distinct.len(), 2, "two cliques, two communities");
        for id in &clique_x {
            assert_eq!(
                community[pos_of(&graph, *id)],
                community[pos_of(&graph, clique_x[0])],
                "clique X stays together"
            );
        }
        for id in &clique_y {
            assert_eq!(
                community[pos_of(&graph, *id)],
                community[pos_of(&graph, clique_y[0])],
                "clique Y stays together"
            );
        }
        // The bridge endpoints stay with their own cliques.
        assert_ne!(
            community[pos_of(&graph, clique_x[4])],
            community[pos_of(&graph, clique_y[4])]
        );
    }

    #[test]
    fn self_loops_do_not_vote_and_duplicate_edges_collapse() {
        // A recursive symbol plus duplicated references lands in the same
        // community as the plain caller pair: the self-vote skip keeps
        // recursion from locking the label, and the graph dedup keeps
        // multi-edges from double-counting.
        let (_dir, rec_conn) = db();
        let rec = insert_symbol(&rec_conn, "rec", "function", "src/rec.rs", 1);
        insert_symbol(&rec_conn, "peer", "function", "src/rec.rs", 2);
        insert_ref(&rec_conn, "rec", Some(rec));
        insert_ref(&rec_conn, "peer", Some(rec));
        insert_ref(&rec_conn, "peer", Some(rec));
        let rec_community = detect_communities(&load_graph(&rec_conn).unwrap(), 30);

        let (_dir2, plain_conn) = db();
        let caller = insert_symbol(&plain_conn, "caller", "function", "src/a.rs", 1);
        insert_symbol(&plain_conn, "callee", "function", "src/a.rs", 2);
        insert_ref(&plain_conn, "callee", Some(caller));
        let plain_community = detect_communities(&load_graph(&plain_conn).unwrap(), 30);

        assert_eq!(rec_community, vec![rec, rec]);
        assert_eq!(
            rec_community, plain_community,
            "recursion and duplicate edges change nothing"
        );
    }

    #[test]
    fn community_passes_caps_the_sweep_count() {
        // s4 calls s1 and s2; s5 calls s1, s2, and s3. After ONE sweep s3
        // still carries its own label (its only neighbor s5 sits at a
        // higher position and has not been swept yet); converged, the
        // graph is one community. The cap is a real bound.
        let (_dir, conn) = db();
        let s: Vec<i64> = (1..=5)
            .map(|i| insert_symbol(&conn, &format!("s{i}"), "function", "src/s.rs", i))
            .collect();
        insert_ref(&conn, "s1", Some(s[3]));
        insert_ref(&conn, "s2", Some(s[3]));
        insert_ref(&conn, "s1", Some(s[4]));
        insert_ref(&conn, "s2", Some(s[4]));
        insert_ref(&conn, "s3", Some(s[4]));
        let graph = load_graph(&conn).unwrap();

        let one_pass = detect_communities(&graph, 1);
        let converged = detect_communities(&graph, 30);
        assert_eq!(one_pass, vec![s[0], s[0], s[2], s[0], s[0]]);
        assert_eq!(converged, vec![s[0]; 5]);
        assert_ne!(one_pass, converged, "the sweep cap is a bound, not a no-op");

        // A converged graph stops early: 5 and 50 sweeps agree exactly.
        let (_dir2, conn2) = db();
        let caller = insert_symbol(&conn2, "caller", "function", "src/a.rs", 1);
        insert_symbol(&conn2, "callee", "function", "src/a.rs", 2);
        insert_ref(&conn2, "callee", Some(caller));
        let graph2 = load_graph(&conn2).unwrap();
        assert_eq!(
            detect_communities(&graph2, 5),
            detect_communities(&graph2, 50)
        );
    }

    #[test]
    fn cohesive_subsystem_groups_together_despite_spanning_directories() {
        let (_dir, conn) = db();
        let tokens = insert_symbol(&conn, "tokens", "function", "src/auth/tokens.rs", 1);
        let session = insert_symbol(&conn, "session", "function", "src/http/session.rs", 1);
        let cookies = insert_symbol(&conn, "cookies", "function", "src/session/cookies.rs", 1);
        let string_utils = insert_symbol(
            &conn,
            "string_utils",
            "function",
            "src/util/string_utils.rs",
            1,
        );
        let math_utils =
            insert_symbol(&conn, "math_utils", "function", "src/util/math_utils.rs", 1);

        // The auth subsystem: each member references the other two, a
        // dense 3-clique across three directories. The utils pair
        // references only each other.
        for (caller, callee) in [
            (tokens, "session"),
            (tokens, "cookies"),
            (session, "tokens"),
            (session, "cookies"),
            (cookies, "tokens"),
            (cookies, "session"),
        ] {
            insert_ref(&conn, callee, Some(caller));
        }
        insert_ref(&conn, "math_utils", Some(string_utils));
        insert_ref(&conn, "string_utils", Some(math_utils));

        recompute(&conn, &opts(20, 30)).unwrap();

        let a = community(&conn, "tokens");
        assert_eq!(a, Some(tokens), "the group key is its MIN id");
        assert_eq!(community(&conn, "session"), a);
        assert_eq!(community(&conn, "cookies"), a);
        assert_eq!(
            community(&conn, "string_utils"),
            Some(string_utils),
            "the disconnected pair keeps its own group"
        );
        assert_ne!(community(&conn, "string_utils"), a);
        let nulls: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM symbol_topology WHERE community IS NULL",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            nulls, 0,
            "every row carries a community, singletons included"
        );
    }

    #[test]
    fn community_ids_stable_across_runs_connections_and_insert_orders() {
        let (_dir_a, conn_a) = db();
        seed_core_vs_leaf(&conn_a);
        recompute(&conn_a, &opts(20, 30)).unwrap();
        let first = communities(&conn_a);

        // Same connection, recomputed.
        recompute(&conn_a, &opts(20, 30)).unwrap();
        assert_eq!(communities(&conn_a), first, "recompute is a pure function");

        // A second database with the same graph.
        let (_dir_b, conn_b) = db();
        seed_core_vs_leaf(&conn_b);
        recompute(&conn_b, &opts(20, 30)).unwrap();
        assert_eq!(
            communities(&conn_b),
            first,
            "identical graph, identical ids"
        );

        // The same graph with references inserted in the opposite order.
        let (_dir_c, conn_c) = db();
        insert_symbol(&conn_c, "core", "class", "src/core.rs", 1);
        insert_symbol(&conn_c, "leaf", "function", "src/leaf.rs", 1);
        let orchestrator = insert_symbol(&conn_c, "orchestrator", "function", "src/orch.rs", 1);
        let single_caller = insert_symbol(&conn_c, "single_caller", "function", "src/single.rs", 1);
        let mut callers = Vec::new();
        for i in 0..5 {
            callers.push(insert_symbol(
                &conn_c,
                &format!("c{i}"),
                "function",
                &format!("src/c{i}.rs"),
                1,
            ));
        }
        for caller in callers.iter().rev() {
            insert_ref(&conn_c, "core", Some(*caller));
        }
        insert_ref(&conn_c, "leaf", Some(single_caller));
        insert_ref(&conn_c, "leaf", Some(orchestrator));
        insert_ref(&conn_c, "core", Some(orchestrator));
        recompute(&conn_c, &opts(20, 30)).unwrap();
        assert_eq!(
            communities(&conn_c),
            first,
            "insertion order must not move a community id"
        );
    }
}
