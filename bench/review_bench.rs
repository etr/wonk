//! Review benchmark (TASK-085, PRD-REV acceptance: typical diff < 2s).
//!
//! Builds the same synthetic shape as the reach bench (~300 Rust files,
//! ~60k symbols, chain/hub call graphs, tests/ tree) but with real git
//! history: the initial commit is the base state the index reflects, and a
//! typical unstaged diff (~10 files / ~14 symbols) is layered on top —
//! one mid-chain removal with many callers, one signature change, nine
//! uncovered body edits, two test-covered body edits, one addition.
//!
//! A self-check run asserts the expected finding counts and BLOCK verdict
//! BEFORE any timing. Then 25 sweeps of run_review measure p50/p95/p100
//! with reach enabled (gated: p95 < 2s, percentile stated explicitly —
//! reach-bench precedent: never gate an unstated percentile) plus a
//! two-phase split (change detection vs blast+rules) and the ungated
//! reach-disabled contrast. Results land in bench/review-results.md.
//!
//! Run: cargo bench --bench review

mod fixture_config;

mod review_samples;
use review_samples::{percentiles, phase_samples, verify_pairing};

use std::fmt::Write as _;
use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::Instant;

use anyhow::{Context, Result, ensure};
use wonk::review::{ReviewOptions, run_review};
use wonk::types::{ChangeScope, FindingSeverity, ReviewVerdict};

const SRC_FILES: usize = 297;
const FNS_PER_FILE: usize = 200;
const MID_HUBS: usize = 30;
const SWEEPS: usize = 25;

fn main() -> Result<()> {
    verify_pairing();
    let repo = tempfile::tempdir()?;
    let root = repo.path();

    let gen_start = Instant::now();
    generate_repo(root)?;
    let generated = gen_start.elapsed();

    git(root, &["init", "-b", "main"])?;
    git(root, &["config", "user.email", "bench@wonk.test"])?;
    git(root, &["config", "user.name", "bench"])?;
    git(root, &["add", "."])?;
    git(root, &["commit", "-m", "base"])?;
    // The index now reflects the base state — the diff's old side.
    let build_start = Instant::now();
    let stats = fixture_config::build_index(root, true)?;
    let build = build_start.elapsed();
    ensure!(
        stats.symbol_count > 55_000 && stats.symbol_count < 70_000,
        "expected ~60k symbols, indexed {}",
        stats.symbol_count
    );

    let index_path = wonk::db::local_index_path(root);
    let conn = wonk::db::open_existing(&index_path)?;

    let diff_start = Instant::now();
    apply_typical_diff(root)?;
    let diff_applied = diff_start.elapsed();

    let options = ReviewOptions::default();
    let scope = ChangeScope::Unstaged;

    // Self-check: the fixture must produce the expected review before any
    // timing claim is made.
    let check = run_review(&conn, &scope, root, &options, None)?;
    let blocking = check
        .findings
        .iter()
        .filter(|f| f.severity == FindingSeverity::Blocking)
        .count();
    let warnings = check
        .findings
        .iter()
        .filter(|f| f.severity == FindingSeverity::Warning)
        .count();
    let removed_rule = check
        .findings
        .iter()
        .any(|f| f.rule == "breaking-change/removed-symbol-with-callers");
    let signature_rule = check
        .findings
        .iter()
        .any(|f| f.rule == "breaking-change/signature-changed-with-callers");
    ensure!(
        check.verdict == ReviewVerdict::Block,
        "expected BLOCK, got {:?} ({:?})",
        check.verdict,
        check
            .findings
            .iter()
            .map(|f| f.rule.clone())
            .collect::<Vec<_>>()
    );
    ensure!(
        blocking == 2,
        "expected 2 blocking findings, got {blocking}"
    );
    // 9 uncovered body edits + the addition + the signature-changed fn
    // (its own depth-3 radius also has no tests — it warns as well).
    ensure!(warnings == 11, "expected 11 warnings, got {warnings}");
    ensure!(removed_rule, "removal finding missing");
    ensure!(signature_rule, "signature-change finding missing");

    // Timed sweeps. The edited files never match their indexed hashes, so
    // the content-hash fast path still does real work every sweep.
    let mut full_ms: Vec<f64> = Vec::with_capacity(SWEEPS);
    let mut detect_ms: Vec<f64> = Vec::with_capacity(SWEEPS);
    for _ in 0..SWEEPS {
        let t = Instant::now();
        let result = run_review(&conn, &scope, root, &options, None)?;
        full_ms.push(t.elapsed().as_secs_f64() * 1000.0);
        ensure!(
            result.findings.len() == check.findings.len(),
            "finding count drifted between sweeps: {} vs {}",
            result.findings.len(),
            check.findings.len()
        );

        let t = Instant::now();
        wonk::impact::detect_changes_detail(&conn, &scope, root)?;
        detect_ms.push(t.elapsed().as_secs_f64() * 1000.0);
    }
    let (f50, f95, f99, f100) = percentiles(&full_ms);
    let (d50, d95, d99, d100) = percentiles(&detect_ms);
    // Two-phase split: paired per-iteration deltas (full_i - detect_i,
    // measured back to back in the same iteration), not subtracted
    // percentiles — those would be noise at this magnitude. Individual
    // deltas can dip below zero from ordering effects; preserve them in
    // both the raw samples and the percentile summary.
    let paired_deltas: Vec<f64> = phase_samples(&full_ms, &detect_ms);
    let (r50, r95, r99, r100) = percentiles(&paired_deltas);

    ensure!(
        f95 < 2000.0,
        "typical-diff review p95 was {f95:.0}ms (acceptance: < 2s with reach enabled)"
    );

    // Ungated contrast: reach disabled — same findings via live BFS only.
    let mut bfs_ms: Vec<f64> = Vec::with_capacity(SWEEPS);
    let bfs_options = ReviewOptions {
        reach_enabled: false,
        ..ReviewOptions::default()
    };
    for _ in 0..SWEEPS {
        let t = Instant::now();
        let result = run_review(&conn, &scope, root, &bfs_options, None)?;
        bfs_ms.push(t.elapsed().as_secs_f64() * 1000.0);
        ensure!(
            result.findings == check.findings,
            "reach kill switch changed findings"
        );
    }
    let (b50, b95, b99, b100) = percentiles(&bfs_ms);

    println!("== review bench (TASK-085) ==");
    println!(
        "repo: {SRC_FILES} src files + mids/util + 3 test files, {FNS_PER_FILE} fns/file, {} symbols (real git history)",
        stats.symbol_count
    );
    println!("generation:       {generated:.2?}");
    println!("index build:      {build:.2?}");
    println!("diff (8 files, 14 changed symbols): {diff_applied:.2?}");
    println!(
        "self-check:       {} blocking + {warnings} warnings -> BLOCK",
        blocking
    );
    println!();
    println!(
        "reach enabled:    p50 {f50:8.2}ms  p95 {f95:8.2}ms  p99 {f99:8.2}ms  p100 {f100:8.2}ms  ({SWEEPS} sweeps)  [gate: p95 < 2000ms]"
    );
    println!(
        "  change detect:  p50 {d50:8.2}ms  p95 {d95:8.2}ms  p99 {d99:8.2}ms  p100 {d100:8.2}ms  (separate sweeps)"
    );
    println!(
        "  blast+rules:    p50 {r50:8.2}ms  p95 {r95:8.2}ms  p99 {r99:8.2}ms  p100 {r100:8.2}ms  (paired per-iteration deltas)"
    );
    println!(
        "reach disabled:   p50 {b50:8.2}ms  p95 {b95:8.2}ms  p99 {b99:8.2}ms  p100 {b100:8.2}ms  (ungated contrast, identical findings)"
    );
    let samples = serde_json::json!({
        "warmups": 1,
        "samples": SWEEPS,
        "full_ms": full_ms,
        "detect_ms": detect_ms,
        "paired_deltas_ms": phase_samples(&full_ms, &detect_ms),
        "bfs_ms": bfs_ms,
        "pairing": "full_i - detect_i before sorting; percentile calculations sort copies"
    });
    std::fs::write(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("bench/review-samples.json"),
        format!("{}\n", serde_json::to_string_pretty(&samples)?),
    )?;
    Ok(())
}

fn git(root: &Path, args: &[&str]) -> Result<()> {
    let out = Command::new("git").args(args).current_dir(root).output()?;
    anyhow::ensure!(
        out.status.success(),
        "git {:?}: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    Ok(())
}

/// The typical diff: mid-chain removal (many callers), signature change,
/// nine uncovered body edits, two test-covered body edits, one addition.
fn apply_typical_diff(root: &Path) -> Result<()> {
    // Removal: mid_1 is called by mid_2 plus the tail fns of ~10 chain files.
    let mids_path = root.join("src/mids.rs");
    let mids = fs::read_to_string(&mids_path)?;
    fs::write(
        &mids_path,
        mids.replace("pub fn mid_1() -> u32 { mid_0() }\n", ""),
    )
    .context("editing mids.rs")?;

    // Signature change with a surviving caller.
    edit(
        root,
        "src/mod_5.rs",
        "pub fn f5_50() -> u32",
        "pub fn f5_50() -> u64",
    )?;

    // Nine uncovered body edits (no test inside their depth-3 radius).
    for m in 1..4 {
        for j in [10, 60, 110] {
            let needle = format!("pub fn f{m}_{j}() -> u32 {{");
            let original = format!("{needle} f{m}_{}() }}", j + 1);
            let edited = format!("{needle} f{m}_{}() + 1 }}", j + 1);
            edit(root, &format!("src/mod_{m}.rs"), &original, &edited)?;
        }
    }

    // Two test-covered body edits (tests call f0_0 / f4_0 directly).
    edit(root, "src/mod_0.rs", "{ f0_1() }", "{ f0_1() + 1 }")?;
    edit(root, "src/mod_4.rs", "{ f4_1() }", "{ f4_1() + 1 }")?;

    // One addition with an empty blast radius.
    let mod6 = root.join("src/mod_6.rs");
    let content = fs::read_to_string(&mod6)?;
    fs::write(
        &mod6,
        format!("{content}pub fn fresh_added() -> u32 {{ 0 }}\n"),
    )
    .context("adding fresh_added")?;
    Ok(())
}

/// Replace the single occurrence of `original` in `rel` with `edited`.
fn edit(root: &Path, rel: &str, original: &str, edited: &str) -> Result<()> {
    let path = root.join(rel);
    let content = fs::read_to_string(&path)?;
    let count = content.matches(original).count();
    anyhow::ensure!(
        count == 1,
        "expected exactly one occurrence of {original:?} in {rel}, found {count}"
    );
    fs::write(&path, content.replacen(original, edited, 1))
        .with_context(|| format!("editing {rel}"))
}

/// Same synthetic shape as the reach bench: per-file chains, tenth-fn hub
/// calls, a mid-tier hub chain, one global hub, and tests/ calling the
/// head of each chain.
fn generate_repo(root: &Path) -> Result<()> {
    fs::create_dir(root.join("src"))?;
    fs::create_dir(root.join("tests"))?;

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
