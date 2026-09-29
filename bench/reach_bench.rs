//! Reach-table benchmark and OQ-012 measurement (TASK-080).
//!
//! Builds a synthetic large repo (~300 Rust files, ~60k symbols, hub/mid/leaf
//! call shapes, impl blocks, tests/ tree), then measures:
//!   1. depth-3 blast latency on the table path (p50/p95/p100, 20 sampled
//!      symbols x 25 sweeps) with a hard p100 < 50ms gate,
//!   2. the live-BFS contrast on the same samples,
//!   3. build cost with reach enabled vs disabled,
//!   4. table size (rows, truncated sources, bytes via dbstat, bytes/symbol)
//!      at the default cap and uncapped.
//!   5. TASK-081 incremental repair cost: reindex_file latency with the
//!      table fresh (repair runs) vs stale (repair skipped) on four edit
//!      shapes — leaf file, chain-calling-hub file, mids, util. The gate
//!      is PER-SHAPE p95 < 50ms: PRD-DMN-REQ-009 states no percentile, so
//!      the gate claims p95 explicitly and p50/p99/p100 are reported
//!      ungated (no pooled gate — pooling dilutes the worst shape).
//!      Shapes whose rebuild set exceeds MAX_INCREMENTAL_REPAIR_SOURCES
//!      trip the work-budget guard and degrade to the stale/BFS path
//!      (PRD-REACH-REQ-007); their measured reindex is the degraded
//!      cost, and the table is rebuilt between iterations (unmeasured)
//!      so every iteration measures the same edit.
//!
//! Results are recorded in bench/reach-results.md. Not a pass/fail gate
//! beyond the latency asserts. Run: cargo bench --bench reach.

use std::fmt::Write as _;
use std::fs;
use std::path::Path;
use std::time::Instant;

use anyhow::{Result, ensure};
use wonk::blast::{BlastOptions, analyze_blast};
use wonk::db;
use wonk::reach::{DEFAULT_MAX_TARGETS_PER_SOURCE, ReachBuildOptions, build_reach};
use wonk::types::BlastDirection;

const SRC_FILES: usize = 297;
const FNS_PER_FILE: usize = 200;
const MID_HUBS: usize = 30;
const SWEEPS: usize = 25;
const BFS_SWEEPS: usize = 5;
const REINDEX_ITERS: usize = 15;

fn main() -> Result<()> {
    let repo = tempfile::tempdir()?;
    let root = repo.path();
    fs::create_dir(root.join(".git"))?;
    fs::create_dir(root.join("src"))?;
    fs::create_dir(root.join("tests"))?;
    let wonk_dir = root.join(".wonk");
    fs::create_dir(&wonk_dir)?;

    let gen_start = Instant::now();
    generate_repo(root)?;
    let generated = gen_start.elapsed();

    // 1. Structural build with reach disabled (V4 baseline cost).
    fs::write(wonk_dir.join("config.toml"), "[reach]\nenabled = false\n")?;
    let t0 = Instant::now();
    let stats = wonk::pipeline::build_index(root, true)?;
    let build_no_reach = t0.elapsed();
    ensure!(
        stats.symbol_count > 55_000 && stats.symbol_count < 70_000,
        "expected ~60k symbols, indexed {}",
        stats.symbol_count
    );

    // 2. Rebuild with reach enabled (default config) — the delta is the
    //    precomputation cost.
    fs::remove_file(wonk_dir.join("config.toml"))?;
    let t1 = Instant::now();
    wonk::pipeline::build_index(root, true)?;
    let build_with_reach = t1.elapsed();

    let index_path = db::local_index_path(root);
    let conn = db::open_existing(&index_path)?;

    // 3. Sampled depth-3 latency: hub (fan-out capped), mid hubs, leaves.
    let mut samples: Vec<String> = vec!["util_trace".to_string()];
    for k in 0..5 {
        samples.push(format!("mid_{k}"));
    }
    for i in 0..14 {
        let m = i * SRC_FILES / 14;
        samples.push(format!("f{m}_0"));
    }
    for name in &samples {
        ensure!(
            wonk::reach::lookup_upstream(&conn, name, 3)?.is_some(),
            "table must cover sample {name}"
        );
    }
    let table_opts = BlastOptions {
        depth: 3,
        direction: BlastDirection::Upstream,
        use_reach: true,
        ..Default::default()
    };
    let hub_truncated = analyze_blast(&conn, "util_trace", &table_opts)?.truncated;
    ensure!(hub_truncated, "the hub's 6k+ fan-out must hit the cap");

    let mut table_ms: Vec<f64> = Vec::new();
    for _ in 0..SWEEPS {
        for name in &samples {
            let start = Instant::now();
            analyze_blast(&conn, name, &table_opts)?;
            table_ms.push(start.elapsed().as_secs_f64() * 1000.0);
        }
    }
    let (t_p50, t_p95, t_p99, t_p100) = percentiles(&mut table_ms);
    ensure!(
        t_p100 < 50.0,
        "depth-3 table-path p100 was {t_p100:.2}ms (acceptance: < 50ms)"
    );

    // 4. Live-BFS contrast on the same samples.
    let bfs_opts = BlastOptions {
        depth: 3,
        direction: BlastDirection::Upstream,
        use_reach: false,
        ..Default::default()
    };
    let mut bfs_ms: Vec<f64> = Vec::new();
    for _ in 0..BFS_SWEEPS {
        for name in &samples {
            let start = Instant::now();
            analyze_blast(&conn, name, &bfs_opts)?;
            bfs_ms.push(start.elapsed().as_secs_f64() * 1000.0);
        }
    }
    let (b_p50, b_p95, b_p99, b_p100) = percentiles(&mut bfs_ms);

    // 5. Table size at the default cap, then uncapped.
    let capped = table_size(&conn)?;
    let uncapped_start = Instant::now();
    {
        let tx = conn.unchecked_transaction()?;
        build_reach(
            &tx,
            &ReachBuildOptions {
                depth: 3,
                max_targets: usize::MAX,
            },
        )?;
        tx.commit()?;
    }
    let uncapped_build = uncapped_start.elapsed();
    let uncapped = table_size(&conn)?;
    let symbols = stats.symbol_count as f64;

    println!("== reach bench (OQ-012) ==");
    println!(
        "repo: {SRC_FILES} src files + mids/util + 3 test files, {FNS_PER_FILE} fns/file, {} symbols",
        stats.symbol_count
    );
    println!("generation:            {generated:.2?}");
    println!("build, reach off:      {build_no_reach:.2?}");
    println!(
        "build, reach on:       {build_with_reach:.2?} (delta {:.2?})",
        build_with_reach.saturating_sub(build_no_reach)
    );
    println!(
        "table path depth-3:    p50 {t_p50:.3}ms  p95 {t_p95:.3}ms  p99 {t_p99:.3}ms  p100 {t_p100:.3}ms  ({} samples)",
        table_ms.len()
    );
    println!(
        "bfs contrast depth-3:  p50 {b_p50:.3}ms  p95 {b_p95:.3}ms  p99 {b_p99:.3}ms  p100 {b_p100:.3}ms  ({} samples)",
        bfs_ms.len()
    );
    println!(
        "table @ cap {DEFAULT_MAX_TARGETS_PER_SOURCE}:  {} rows, {} truncated sources, {:.2} MiB, {:.1} B/symbol",
        capped.rows,
        capped.truncated,
        capped.bytes as f64 / (1024.0 * 1024.0),
        capped.bytes as f64 / symbols
    );
    println!(
        "table uncapped:        {} rows, {} truncated sources, {:.2} MiB, {:.1} B/symbol (rebuild {:.2?})",
        uncapped.rows,
        uncapped.truncated,
        uncapped.bytes as f64 / (1024.0 * 1024.0),
        uncapped.bytes as f64 / symbols,
        uncapped_build
    );

    // 6. TASK-081: incremental repair cost per edit shape. The uncapped
    //    experiment above rebuilt the table; restore the default-capped
    //    state the repair assumes before measuring.
    rebuild_reach_default(&conn)?;

    // A leaf file nothing else references, indexed through the daemon's
    // new-file path after the build.
    let leaf = root.join("src/leaf.rs");
    fs::write(
        &leaf,
        "pub fn leaf_a() -> u32 {\n    1\n}\npub fn leaf_b() -> u32 {\n    leaf_a()\n}\n",
    )?;
    wonk::pipeline::reindex_file(&conn, &leaf, root)?;

    // (label, rel path, a covered sample name) per edit shape. mod_0.rs is
    // the chain-calling-hub shape: its edit's reverse lookup pulls in the
    // util_trace hub recompute; mids.rs touches the 30 mid-tier sources;
    // util.rs rewrites the global hub's own source row set.
    let shapes: &[(&str, &str, &str)] = &[
        ("leaf", "src/leaf.rs", "leaf_a"),
        ("chain-hub", "src/mod_0.rs", "f0_0"),
        ("mids", "src/mids.rs", "mid_0"),
        ("util", "src/util.rs", "util_trace"),
    ];

    println!();
    println!(
        "== TASK-081: incremental repair ({} iters/shape, work-budget guard {} sources) ==",
        REINDEX_ITERS,
        wonk::reach::MAX_INCREMENTAL_REPAIR_SOURCES
    );

    // On phase, table fresh. Shapes within the guard budget repair
    // incrementally. Shapes over it are refused before any writes and
    // degrade to the stale/BFS path (PRD-REACH-REQ-007): their measured
    // reindex is the degraded cost (rebuild-set computation + refusal +
    // stale marker + commit), and the table is rebuilt between
    // iterations — outside the measured window — so every iteration
    // measures the same edit.
    let mut edit_counter = 0usize;
    let mut on_ms: Vec<(&str, Vec<f64>)> = Vec::new();
    let mut tripped: Vec<bool> = Vec::new();
    for (label, rel, sample) in shapes {
        let path = root.join(rel);
        let base = fs::read_to_string(&path)?;
        let mut samples = Vec::with_capacity(REINDEX_ITERS);
        let mut trips = 0usize;
        for _ in 0..REINDEX_ITERS {
            edit_counter += 1;
            fs::write(&path, format!("{base}// bench edit {edit_counter}\n"))?;
            let start = Instant::now();
            wonk::pipeline::reindex_file(&conn, &path, root)?;
            samples.push(start.elapsed().as_secs_f64() * 1000.0);
            if reach_stale(&conn)? {
                trips += 1;
                ensure!(
                    wonk::reach::lookup_upstream(&conn, sample, 3)?.is_none(),
                    "{label}: a guard-tripped (stale) table must not answer"
                );
                rebuild_reach_default(&conn)?;
            } else {
                ensure!(
                    wonk::reach::lookup_upstream(&conn, sample, 3)?.is_some(),
                    "{label}: lookup must stay Some after every repair"
                );
            }
        }
        // Comment-only edits over an identical graph: the guard's verdict
        // must be the same on every iteration.
        ensure!(
            trips == 0 || trips == REINDEX_ITERS,
            "{label}: guard tripped {trips}/{} iterations; expected all or none",
            REINDEX_ITERS
        );
        tripped.push(trips == REINDEX_ITERS);
        on_ms.push((label, samples));
    }

    // The same edits with the table stale: begin/finish skip the repair,
    // so the paired on/off contrast isolates the repair's own cost for
    // within-budget shapes (and shows the tripping shapes' steady-state
    // daemon cost — once the guard trips, every later edit takes this
    // skip path until a full rebuild). (The table falls behind during
    // this phase; it is only measuring.)
    {
        let tx = conn.unchecked_transaction()?;
        wonk::reach::mark_stale(&tx)?;
        tx.commit()?;
    }
    let mut off_ms: Vec<(&str, Vec<f64>)> = Vec::new();
    for (label, rel, _sample) in shapes {
        let path = root.join(rel);
        let base = fs::read_to_string(&path)?;
        let mut samples = Vec::with_capacity(REINDEX_ITERS);
        for _ in 0..REINDEX_ITERS {
            edit_counter += 1;
            fs::write(&path, format!("{base}// bench edit {edit_counter}\n"))?;
            let start = Instant::now();
            wonk::pipeline::reindex_file(&conn, &path, root)?;
            samples.push(start.elapsed().as_secs_f64() * 1000.0);
            ensure!(
                reach_stale(&conn)?,
                "{label}: stale marker must persist while skipped"
            );
        }
        off_ms.push((label, samples));
    }

    println!();
    for (i, (label, on)) in on_ms.iter().enumerate() {
        let off = &off_ms[i].1;
        let did_trip = tripped[i];
        let (o_p50, o_p95, o_p99, o_p100) = percentiles(&mut on.clone());
        let (f_p50, f_p95, f_p99, f_p100) = percentiles(&mut off.clone());
        if did_trip {
            println!(
                "{label:<10} guard:      TRIPPED every iteration (rebuild set over budget) \
                 — repair refused, table marked stale, lookups fall back to BFS"
            );
            println!(
                "{label:<10} degraded:   p50 {o_p50:7.2}ms  p95 {o_p95:7.2}ms  p99 {o_p99:7.2}ms  p100 {o_p100:7.2}ms"
            );
        } else {
            let mut deltas: Vec<f64> = on
                .iter()
                .zip(off.iter())
                .map(|(a, b)| (a - b).max(0.0))
                .collect();
            let (d_p50, d_p95, d_p99, d_p100) = percentiles(&mut deltas);
            println!("{label:<10} guard:      within budget (repair runs incrementally)");
            println!(
                "{label:<10} reindex on: p50 {o_p50:7.2}ms  p95 {o_p95:7.2}ms  p99 {o_p99:7.2}ms  p100 {o_p100:7.2}ms"
            );
            println!(
                "{label:<10} repair-only: p50 {d_p50:7.2}ms  p95 {d_p95:7.2}ms  p99 {d_p99:7.2}ms  p100 {d_p100:7.2}ms"
            );
        }
        println!(
            "{label:<10} reindex off: p50 {f_p50:7.2}ms  p95 {f_p95:7.2}ms  p99 {f_p99:7.2}ms  p100 {f_p100:7.2}ms"
        );
        // PRD-DMN-REQ-009 gate, per shape, percentile stated explicitly:
        // p95 of the effective reindex path (incremental when within the
        // work budget, degraded when the guard trips) must be < 50ms.
        let path_kind = if did_trip { "degraded" } else { "incremental" };
        ensure!(
            o_p95 < 50.0,
            "{label}: {path_kind} reindex p95 was {o_p95:.2}ms \
             (PRD-DMN-REQ-009 gate: per-shape p95 < 50ms)"
        );
    }
    Ok(())
}

/// Restore the fresh, default-capped table the repair assumes (depth 3,
/// default fan-out cap) — the bench's unmeasured reset between guard-trip
/// iterations.
fn rebuild_reach_default(conn: &rusqlite::Connection) -> Result<()> {
    let tx = conn.unchecked_transaction()?;
    build_reach(
        &tx,
        &ReachBuildOptions {
            depth: 3,
            max_targets: DEFAULT_MAX_TARGETS_PER_SOURCE,
        },
    )?;
    tx.commit()?;
    Ok(())
}

/// Whether the reach table currently carries the stale marker.
fn reach_stale(conn: &rusqlite::Connection) -> Result<bool> {
    let stale: i64 = conn.query_row(
        "SELECT COUNT(*) FROM reach_meta WHERE key = 'stale'",
        [],
        |r| r.get(0),
    )?;
    Ok(stale > 0)
}

struct TableSize {
    rows: i64,
    truncated: i64,
    bytes: i64,
}

fn table_size(conn: &rusqlite::Connection) -> Result<TableSize> {
    let rows: i64 = conn.query_row("SELECT COUNT(*) FROM reach", [], |r| r.get(0))?;
    let truncated: i64 =
        conn.query_row("SELECT COUNT(*) FROM reach_truncated", [], |r| r.get(0))?;
    let bytes: i64 = conn.query_row(
        "SELECT COALESCE(SUM(pgsize), 0) FROM dbstat \
         WHERE name IN ('reach', 'reach_truncated', 'reach_meta')",
        [],
        |r| r.get(0),
    )?;
    Ok(TableSize {
        rows,
        truncated,
        bytes,
    })
}

fn percentiles(samples: &mut [f64]) -> (f64, f64, f64, f64) {
    samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let n = samples.len();
    let at = |q: f64| samples[((q * (n - 1) as f64).round()) as usize];
    (at(0.50), at(0.95), at(0.99), at(1.0))
}

/// Writes a synthetic repo: per-file function chains, per-10th hub calls,
/// mid-tier hubs, one global hub, impl blocks, and a tests/ tree that calls
/// production code (excluded from the table by the shared predicate).
fn generate_repo(root: &Path) -> Result<()> {
    fs::write(
        root.join("src/util.rs"),
        "pub fn util_trace() -> u32 {\n    1\n}\n",
    )?;

    let mut mids = String::from("pub fn mid_0() -> u32 {\n    util_trace()\n}\n");
    for k in 1..MID_HUBS {
        writeln!(&mut mids, "pub fn mid_{k}() -> u32 {{ mid_{}() }}", k - 1)?;
    }
    fs::write(root.join("src/mids.rs"), mids)?;

    for m in 0..SRC_FILES {
        let mut src = String::with_capacity(FNS_PER_FILE * 64);
        writeln!(&mut src, "pub struct Item{m} {{ v: u32 }}")?;
        writeln!(&mut src, "impl Item{m} {{")?;
        writeln!(
            &mut src,
            "    pub fn new(v: u32) -> Self {{ Item{m} {{ v }} }}"
        )?;
        writeln!(&mut src, "    pub fn get(&self) -> u32 {{ self.v }}")?;
        writeln!(&mut src, "}}")?;
        for j in 0..FNS_PER_FILE {
            let call = if j + 1 < FNS_PER_FILE {
                format!("f{m}_{}", j + 1)
            } else {
                format!("mid_{}", m * MID_HUBS / SRC_FILES)
            };
            if j % 10 == 5 {
                writeln!(
                    &mut src,
                    "pub fn f{m}_{j}() -> u32 {{ util_trace() + {call}() }}"
                )?;
            } else {
                writeln!(&mut src, "pub fn f{m}_{j}() -> u32 {{ {call}() }}")?;
            }
        }
        fs::write(root.join(format!("src/mod_{m}.rs")), src)?;
    }

    for t in 0..3 {
        let mut src = String::with_capacity(FNS_PER_FILE * 64);
        for j in 0..FNS_PER_FILE {
            let m = (t * FNS_PER_FILE + j) % SRC_FILES;
            writeln!(&mut src, "#[test]\nfn t{t}_{j}() {{ let _ = f{m}_0(); }}")?;
        }
        fs::write(root.join(format!("tests/x_{t}.rs")), src)?;
    }
    Ok(())
}
