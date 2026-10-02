//! TASK-102 acceptance tests: contrastive weight learning.
//!
//! End-to-end over a tempdir repo indexed with the real `build_index`,
//! driven both through the real binary (`wonk search --why` /
//! `wonk feedback [--weights]`) and at library level with injected
//! clocks. The fixture is the DR-043 shape: `src/` and `tests/` twin
//! symbols that tie on every recorded signal except `path_character`
//! (1.0 vs 0.2), with the lexical channel weighting the short tests
//! file higher so the tests twin ranks FIRST pre-learning — feedback
//! preferring the implementation is then a genuine rank-≥2 correction.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use rusqlite::Connection;
use serde_json::Value;
use tempfile::TempDir;

use wonk::config::FeedbackConfig;
use wonk::db;
use wonk::feedback;
use wonk::learning;
use wonk::rerank::RankSettings;

// ---------------------------------------------------------------------------
// Fixture
// ---------------------------------------------------------------------------

/// The implementation twin: an ORDINARY (path_character 1.0) src file,
/// long enough that BM25 ranks its single hits low — the initial
/// tests-first order comes from the lexical channel, not from any
/// learned state.
const CROP_IMPL: &str = r#"// Crop accounting for the farm ledger.
pub fn crop_yield(acres: f64, rain: f64) -> f64 {
    let base = acres * rain * 0.01;
    let room = granary_room(base);
    room.min(base)
}

fn granary_room(load: f64) -> f64 {
    load * 0.9
}

pub fn rotate_fields(season: u32) -> u32 {
    season % 4
}

pub fn tilth_index(moisture: f64) -> f64 {
    moisture.clamp(0.0, 1.0)
}

pub fn sowing_window(latitude: f64) -> f64 {
    latitude.abs() / 90.0
}

pub fn compost_mass(input_kg: f64) -> f64 {
    input_kg * 0.65
}

pub fn furrow_depth(soil: &str) -> f64 {
    match soil {
        "clay" => 0.18,
        "sand" => 0.32,
        _ => 0.24,
    }
}

pub fn windbreak_height(rows: u32) -> f64 {
    rows as f64 * 1.7
}

pub fn irrigation_volume(area: f64, days: u32) -> f64 {
    area * days as f64 * 4.1
}

pub fn pest_pressure(count: u32) -> u32 {
    count / 3
}

pub fn harvest_moisture(sample: f64) -> f64 {
    sample * 0.82
}

pub fn terrace_width(slope: f64) -> f64 {
    (1.0 - slope) * 6.0
}

pub fn mulch_cover(area: f64) -> f64 {
    area * 0.4
}

pub fn drain_flow(rain: f64, area: f64) -> f64 {
    rain * area * 0.55
}

pub fn seed_viability(age_years: u32) -> f64 {
    1.0 / (1.0 + age_years as f64)
}

pub fn pollinator_count(hives: u32) -> u32 {
    hives * 900
}

pub fn erosion_factor(slope: f64, cover: f64) -> f64 {
    slope * (1.0 - cover)
}
"#;

/// The test twin: a TEST-classified (path_character 0.2) file, short and
/// term-dense so its BM25 scores lead.
const CROP_TEST: &str = r#"// crop_yield checks against the granary budget.
use farm::crop::crop_yield;

fn crop_yield_stub() -> f64 {
    crop_yield(1.0, 2.0)
}

fn granary_budget_check() -> bool {
    crop_yield(0.0, 0.0).is_finite()
}
"#;

/// A second ordinary src file, lexically dense for the query, so the
/// implementation twin's definition sits at slate rank >= 2 — learning
/// only fires where the ranking was wrong (PRD-FB-REQ-009).
const REPORT_RS: &str = r#"// Reporting over crop_yield.
pub fn crop_yield_report(rows: usize) -> f64 {
    let v = crop_yield(1.0, 2.0);
    v * rows as f64
}

pub fn crop_yield_summary() -> String {
    format!("{}", crop_yield(0.0, 1.0))
}
"#;

/// The learning fixture's weight table: kind OFF (otherwise the kind
/// signal's Definition-vs-Test gap dwarfs everything), path_character
/// small, lexical the dominant pre-learning orderer, and the `feedback`
/// channel at its default.
fn fixture_weights() -> HashMap<String, f32> {
    HashMap::from([
        ("path_character".to_string(), 0.1),
        ("lexical".to_string(), 0.4),
        ("feedback".to_string(), 0.35),
    ])
}

fn learning_repo(enabled: bool, extra_config: &str) -> (TempDir, PathBuf) {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("learn-repo");
    fs::create_dir_all(root.join("src/crop")).unwrap();
    fs::create_dir_all(root.join("tests")).unwrap();
    fs::create_dir_all(root.join(".git")).unwrap();
    fs::write(root.join("src/crop/mod.rs"), CROP_IMPL).unwrap();
    fs::write(root.join("src/report.rs"), REPORT_RS).unwrap();
    fs::write(root.join("tests/crop_test.rs"), CROP_TEST).unwrap();
    fs::create_dir_all(root.join(".wonk")).unwrap();
    fs::write(
        root.join(".wonk/config.toml"),
        format!(
            "[feedback]\nenabled = {enabled}\n{extra_config}\n\
             \n[rank]\nenabled = true\n\n[rank.weights]\n\
             path_character = 0.1\nlexical = 0.4\nfeedback = 0.35\n"
        ),
    )
    .unwrap();
    wonk::pipeline::build_index(&root, true).unwrap();
    (dir, root)
}

fn open_index(root: &Path) -> Connection {
    let path = db::find_existing_index(root).expect("fixture index to exist");
    db::open(&path).unwrap()
}

fn wonk_bin() -> PathBuf {
    let mut path = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    path.push("wonk");
    path
}

fn run_wonk(root: &Path, args: &[&str]) -> (i32, String, String) {
    let out = Command::new(wonk_bin())
        .arg("--quiet")
        .args(args)
        .current_dir(root)
        .output()
        .unwrap();
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn json_rows(stdout: &str) -> Vec<Value> {
    stdout
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

/// Search stdout minus the `slate:` plumbing line — the enabled repo
/// records a slate, the disabled one cannot; RANKING is what must match.
fn result_lines(stdout: &str) -> Vec<&str> {
    stdout.lines().filter(|l| !l.starts_with("slate: ")).collect()
}

fn feedback_config(enabled: bool) -> FeedbackConfig {
    FeedbackConfig {
        enabled,
        ..FeedbackConfig::default()
    }
}

/// The ranked search the dispatch layer would hold, with capture and the
/// learned overlay both threaded — the `[feedback] enabled` search shape.
fn ranked_for(root: &Path, conn: &Connection, query: &str, learned: Option<learning::LearnedTable>) -> wonk::rerank::RankedSearch {
    let root_str = root.display().to_string();
    let mut results = wonk::search::text_search(query, false, false, &[root_str]).unwrap();
    for result in &mut results {
        if let Ok(rel) = result.file.strip_prefix(root) {
            result.file = rel.to_path_buf();
        }
    }
    let settings = RankSettings {
        use_pipeline: true,
        weights: wonk::rerank::WeightTable::from_config(&fixture_weights()).unwrap(),
        feedback_capture: true,
        learned,
        ..RankSettings::default()
    };
    wonk::rerank::rank_and_explain_classed(&results, Some(conn), query, &settings)
}

/// Record one useful-identity event from a real search and learn from
/// everything pending. Returns the identity used.
fn record_and_learn(root: &Path, conn: &Connection, query: &str, pick: Pick, session: &str, now: i64) -> String {
    let ranked = ranked_for(root, conn, query, None);
    let stored =
        feedback::build_and_store_slate(conn, query, &ranked, &feedback_config(true)).unwrap();
    let identity = match pick {
        Pick::SrcImpl => stored
            .members
            .iter()
            .find(|m| m.file.ends_with("src/crop/mod.rs"))
            .expect("src twin in slate")
            .identity
            .clone(),
        Pick::Rank(rank) => stored
            .members
            .iter()
            .find(|m| m.rank == rank)
            .expect("rank in slate")
            .identity
            .clone(),
    };
    feedback::record_feedback(conn, &stored.token, &[identity.clone()], session).unwrap();
    learning::learn_pending(conn, &feedback_config(true), &fixture_weights(), now).unwrap();
    identity
}

#[derive(Clone, Copy)]
enum Pick {
    /// The first result that lives in `src/`.
    SrcImpl,
    /// The member at a 1-based rank.
    Rank(usize),
}

/// Flattened (file, line) order of a ranked search.
fn flat_positions(ranked: &wonk::rerank::RankedSearch) -> Vec<(String, u64)> {
    ranked
        .groups
        .iter()
        .flat_map(|(_, group)| group.iter())
        .map(|item| {
            (
                item.classified.result.file.to_string_lossy().into_owned(),
                item.classified.result.line,
            )
        })
        .collect()
}

/// Flattened (file, line) of a JSON search's rows.
fn row_positions(rows: &[Value]) -> Vec<(String, u64)> {
    rows.iter()
        .map(|row| {
            (
                row["file"].as_str().unwrap().to_string(),
                row["line"].as_u64().unwrap(),
            )
        })
        .collect()
}

// ---------------------------------------------------------------------------
// AC: preferring implementation over tests shifts path_character,
// visibly, with its counts
// ---------------------------------------------------------------------------

#[test]
fn preferring_implementation_shifts_path_character_visibly() {
    let (dir, root) = learning_repo(true, "");

    // Pre-learning: the tests twin OUTSCORES the implementation twin on
    // the lexical channel (the display tiers stay structural; scores are
    // where the preference lives).
    let (_, _, why_pre) = run_wonk(&root, &["search", "--include-tests", "--why", "crop_yield"]);
    let pre = why_totals(&why_pre);
    let (pre_src, pre_tests) = (
        total_of(&pre, "src/crop/mod.rs").expect("src twin scored"),
        max_total_of(&pre, "tests/crop_test.rs").expect("tests twin scored"),
    );
    assert!(pre_tests > pre_src, "tests-first scores: {pre:?}");

    // 40 feedback events preferring the src implementation across 40
    // distinct sessions — past every gate.
    let (code, stdout, stderr) = run_wonk(
        &root,
        &["search", "--include-tests", "--format", "json", "crop_yield"],
    );
    assert_eq!(code, 0, "stderr: {stderr}");
    let rows = json_rows(&stdout);
    let src_identity = rows
        .iter()
        .find(|row| row["file"].as_str().unwrap().ends_with("src/crop/mod.rs"))
        .and_then(|row| row["identity"].as_str())
        .expect("identity on rows")
        .to_string();
    let slate = rows[0]["slate"].as_str().expect("slate on rows").to_string();
    for n in 0..40 {
        let (code, _, err) = run_wonk(
            &root,
            &[
                "feedback",
                "--slate",
                &slate,
                "--useful",
                &src_identity,
                "--session",
                &format!("session-{n:02}"),
            ],
        );
        assert_eq!(code, 0, "{err}");
    }

    // The named number against its default, with its counts.
    let (code, out, err) = run_wonk(&root, &["feedback", "--weights"]);
    assert_eq!(code, 0, "{err}");
    let expected = "path_character [overall] 0.150 (default 0.100) 40 obs, 40 sessions";
    assert!(
        out.contains(expected),
        "the shifted weight, at its bound, with counts: {out}"
    );
    assert!(
        !out.contains(&format!("{expected} [below gate]")),
        "a gated, shifted row must not be marked inert: {out}"
    );

    // The `learned:` line under --why.
    let (code, _, why_stderr) = run_wonk(&root, &["search", "--include-tests", "--why", "crop_yield"]);
    assert_eq!(code, 0);
    assert!(
        why_stderr.contains("learned: "),
        "the learned line prints under --why: {why_stderr}"
    );
    assert!(
        why_stderr.contains("path_character 0.150 (default 0.100, 40 obs/40 sessions)"),
        "path_character rides the learned line with its counts: {why_stderr}"
    );

    // The shifted preference reorders the ORIGINAL query's scores.
    let (_, _, why_post) = run_wonk(&root, &["search", "--include-tests", "--why", "crop_yield"]);
    let post = why_totals(&why_post);
    let (post_src, post_tests) = (
        total_of(&post, "src/crop/mod.rs").unwrap(),
        max_total_of(&post, "tests/crop_test.rs").unwrap(),
    );
    assert!(
        post_src > post_tests,
        "learning reorders the source query's scores: src {post_src} vs tests {post_tests}"
    );
    drop(dir);
}

/// `--why` stderr → ((file, total)) per result line.
fn why_totals(stderr: &str) -> Vec<(String, f32)> {
    stderr
        .lines()
        .filter(|l| l.starts_with("why: "))
        .filter_map(|l| {
            let body = l.trim_start_matches("why: ");
            let (file, total) = body.split_once(" total=")?;
            let total = total.split_whitespace().next()?;
            Some((file.to_string(), total.parse::<f32>().ok()?))
        })
        .collect()
}

/// The total of the FIRST `--why` line of `suffix` (the twin's own line).
fn total_of(totals: &[(String, f32)], suffix: &str) -> Option<f32> {
    totals
        .iter()
        .find(|(file, _)| file.starts_with(suffix))
        .map(|(_, total)| *total)
}

/// The LARGEST total among `suffix`'s lines.
fn max_total_of(totals: &[(String, f32)], suffix: &str) -> Option<f32> {
    totals
        .iter()
        .filter(|(file, _)| file.starts_with(suffix))
        .map(|(_, total)| *total)
        .fold(None, |acc, v| Some(acc.map_or(v, |m: f32| m.max(v))))
}

// ---------------------------------------------------------------------------
// AC: the shift generalizes to a term-disjoint query
// ---------------------------------------------------------------------------

#[test]
fn shift_generalizes_to_a_term_disjoint_query() {
    let (dir, root) = learning_repo(true, "");
    let conn = open_index(&root);

    let a_terms = wonk::tokenizer::tokenize("crop_yield");
    let b_terms = wonk::tokenizer::tokenize("granary");
    assert!(
        a_terms.iter().all(|a| !b_terms.contains(a)),
        "queries must be term-disjoint: {a_terms:?} vs {b_terms:?}"
    );

    // Pre-learning: the tests file's granary lines OUTSCORE the src ones.
    let pre = ranked_for(&root, &conn, "granary", None);
    let (pre_src, pre_tests) = (
        max_score_of(&pre, "src/crop/mod.rs"),
        max_score_of(&pre, "tests/crop_test.rs"),
    );
    assert!(
        pre_tests > pre_src,
        "tests-first pre-learning: src {pre_src:?} vs tests {pre_tests:?}"
    );

    // Learn from crop_yield only — 40 events across 40 sessions.
    for n in 0..40 {
        record_and_learn(&root, &conn, "crop_yield", Pick::SrcImpl, &format!("s{n}"), 1000);
    }

    // Post-learning: the src lines outscore the tests lines on the
    // term-disjoint query — the property item-keyed feedback cannot
    // deliver.
    let learned = learning::load_learned(&conn, &feedback_config(true), &fixture_weights(), 1000)
        .unwrap()
        .expect("gated learned rows");
    let post = ranked_for(&root, &conn, "granary", Some(learned));
    let (post_src, post_tests) = (
        max_score_of(&post, "src/crop/mod.rs"),
        max_score_of(&post, "tests/crop_test.rs"),
    );
    assert!(
        post_src > post_tests,
        "the shift generalizes: src {post_src:?} vs tests {post_tests:?}"
    );
    drop(conn);
    drop(dir);
}

/// The largest score among the candidates whose file ends with `suffix`.
fn max_score_of(ranked: &wonk::rerank::RankedSearch, suffix: &str) -> Option<f32> {
    ranked
        .groups
        .iter()
        .flat_map(|(_, group)| group.iter())
        .filter(|item| item.classified.result.file.to_string_lossy().ends_with(suffix))
        .map(|item| item.score)
        .fold(None, |acc, v| Some(acc.map_or(v, |m: f32| m.max(v))))
}

// ---------------------------------------------------------------------------
// AC: feedback on an already-first result changes no weight
// ---------------------------------------------------------------------------

#[test]
fn already_first_feedback_changes_no_weight() {
    let (dir, root) = learning_repo(true, "");
    let conn = open_index(&root);

    for n in 0..12 {
        record_and_learn(&root, &conn, "crop_yield", Pick::SrcImpl, &format!("s{n}"), 1000);
    }
    let dump = |conn: &Connection| learned_dump(conn);
    let before = dump(&conn);

    // The learned state flipped the granary query: its rank-1 result is
    // now the src line. Feedback on THAT (already first) must be a no-op.
    let identity = record_and_learn(&root, &conn, "granary", Pick::Rank(1), "late-1", 2000);
    assert!(
        identity.chars().all(|c| c.is_ascii_hexdigit()),
        "a real identity was recorded: {identity}"
    );
    for n in 2..6 {
        record_and_learn(&root, &conn, "granary", Pick::Rank(1), &format!("late-{n}"), 2000);
    }
    assert_eq!(before, dump(&conn), "rank-1 feedback learns nothing");
    drop(conn);
    drop(dir);
}

fn learned_dump(conn: &Connection) -> Vec<(String, String, f32, i64, i64)> {
    let mut stmt = conn
        .prepare(
            "SELECT feature, query_class, weight, observations, sessions \
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
        ))
    })
    .unwrap()
    .collect::<rusqlite::Result<Vec<_>>>()
    .unwrap()
}

// ---------------------------------------------------------------------------
// AC: adversarial repetition cannot push any weight beyond the deviation
// ---------------------------------------------------------------------------

#[test]
fn adversarial_repetition_stays_within_the_deviation() {
    let (dir, root) = learning_repo(true, "");
    let conn = open_index(&root);

    // 500 same-direction events across 4 sessions, driven straight into
    // the events table the way record_feedback writes them.
    let useful = wonk::feedback::SlateMember {
        identity: "u".to_string(),
        rank: 2,
        chosen: true,
        file: "src/crop/mod.rs".to_string(),
        line: 2,
        symbol: Some("crop_yield".to_string()),
        kind: Some("function".to_string()),
        score: 1.0,
        groups: member_groups(1.0),
    };
    let alt = wonk::feedback::SlateMember {
        identity: "a".to_string(),
        rank: 1,
        chosen: false,
        file: "tests/crop_test.rs".to_string(),
        line: 3,
        symbol: Some("crop_yield_stub".to_string()),
        kind: Some("function".to_string()),
        score: 0.9,
        groups: member_groups(0.0),
    };
    let features = wonk::feedback::SlateFeatures {
        schema: 1,
        slate: "adversarial".to_string(),
        members: vec![alt, useful],
    };
    let payload = serde_json::to_string(&features).unwrap();
    for id in 1..=500 {
        conn.execute(
            "INSERT INTO feedback_events \
             (id, result_identity, query_class, chosen_rank, features, useful, session, created_at) \
             VALUES (?1, 'u', NULL, 2, ?2, 1, ?3, ?4)",
            rusqlite::params![id, payload, format!("adv{}", id % 4), id],
        )
        .unwrap();
    }
    learning::learn_pending(&conn, &feedback_config(true), &fixture_weights(), 10_000).unwrap();

    // Stored weights sit AT their bounds, never beyond; no NaN. Signal
    // keys clamp multiplicatively around their configured default
    // (path_character [0.05, 0.15], lexical [0.2, 0.6]); descriptive
    // keys at ±dev, sign following the side they were observed on.
    let rows = learned_dump(&conn);
    assert!(!rows.is_empty());
    for (feature, scope, weight, observations, sessions) in &rows {
        assert_eq!(scope, "");
        assert!(weight.is_finite(), "{feature} went NaN");
        assert_eq!(*observations, 500, "{feature}");
        assert_eq!(*sessions, 4, "{feature}");
        if feature.contains(':') {
            assert!(
                weight.abs() <= 0.5 + 1e-6,
                "descriptive keys within ±dev: {feature} = {weight}"
            );
            let pinned = (*weight - 0.5).abs() < 1e-6 || (*weight + 0.5).abs() < 1e-6;
            assert!(pinned, "500 same-direction events pin the bound: {feature} = {weight}");
        } else {
            let default = fixture_weights().get(feature).copied().unwrap_or(0.0);
            let (lo, hi) = (default * 0.5, default * 1.5);
            assert!(
                *weight >= lo - 1e-6 && *weight <= hi + 1e-6,
                "signal within its multiplicative bound: {feature} = {weight} not in [{lo}, {hi}]"
            );
            let pinned = (*weight - hi).abs() < 1e-6 || (*weight - lo).abs() < 1e-6;
            assert!(pinned, "the bound is REACHED: {feature} = {weight} not at {lo}/{hi}");
        }
    }

    // Tightening the deviation re-bounds the stored values at load time.
    let tightened = FeedbackConfig {
        learn_max_deviation: 0.1,
        ..feedback_config(true)
    };
    let table = learning::load_learned(&conn, &tightened, &fixture_weights(), 10_000)
        .unwrap()
        .expect("still gated");
    for row in table.evidence() {
        // Signals re-bound multiplicatively, descriptive keys by the
        // tightened absolute dev.
        let bound = if row.default != 0.0 {
            row.default.abs() * 0.1
        } else {
            0.1
        };
        assert!(
            (row.effective - row.default).abs() <= bound + 1e-6,
            "tightened bound holds after reload: {row:?}"
        );
    }
    drop(conn);
    drop(dir);
}

fn member_groups(path_character: f32) -> wonk::feedback::FeatureGroups {
    let contribution = |signal: &str, value: f32| wonk::output::ContributionOutput {
        signal: signal.to_string(),
        value,
        weight: 1.0,
        weighted: value,
    };
    let mut groups = wonk::feedback::FeatureGroups::default();
    groups.signals = vec![
        contribution("path_character", path_character),
        contribution("lexical", 1.0 - path_character),
    ];
    if path_character > 0.5 {
        groups.path.insert("src".to_string(), "1".to_string());
        groups.path.insert("class".to_string(), "ordinary".to_string());
    } else {
        groups.path.insert("tests".to_string(), "1".to_string());
        groups.path.insert("class".to_string(), "test".to_string());
    }
    groups
}

// ---------------------------------------------------------------------------
// AC: weights decay toward defaults with age
// ---------------------------------------------------------------------------

#[test]
fn learned_weights_decay_toward_defaults_with_age() {
    let (dir, root) = learning_repo(true, "");
    let conn = open_index(&root);
    for n in 0..12 {
        record_and_learn(&root, &conn, "crop_yield", Pick::SrcImpl, &format!("s{n}"), 1000);
    }

    let at = |now: i64| {
        learning::list_learned(&conn, &feedback_config(true), &fixture_weights(), now)
            .unwrap()
            .into_iter()
            .find(|r| r.feature == "path_character" && r.query_class.is_empty())
            .unwrap()
    };
    let fresh = at(1000);
    assert!(
        (fresh.effective - 0.1).abs() > 0.01,
        "the row actually deviates before decay: {fresh:?}"
    );

    // One half-life (30d): the deviation halves; ten half-lives: gone.
    let aged = at(1000 + 30 * 86_400);
    let halved = 0.1 + (fresh.effective - 0.1) / 2.0;
    assert!(
        (aged.effective - halved).abs() < 1e-4,
        "one half-life halves the deviation: {aged:?} vs {halved}"
    );
    let stale = at(1000 + 300 * 86_400);
    assert!(
        (stale.effective - 0.1).abs() < 1e-3,
        "ten half-lives collapse to the default: {stale:?}"
    );
    drop(conn);
    drop(dir);
}

// ---------------------------------------------------------------------------
// AC: the session/observation gate (AR-044)
// ---------------------------------------------------------------------------

#[test]
fn gates_hold_until_observations_span_sessions() {
    let (dir, root) = learning_repo(true, "");
    let conn = open_index(&root);

    let baseline = flat_positions(&ranked_for(&root, &conn, "crop_yield", None));

    // 5 events, 1 session: no ranking influence.
    for _ in 0..5 {
        record_and_learn(&root, &conn, "crop_yield", Pick::SrcImpl, "only-session", 1000);
    }
    assert!(
        learning::load_learned(&conn, &feedback_config(true), &fixture_weights(), 1000)
            .unwrap()
            .is_none(),
        "5 obs / 1 session stays inert"
    );
    assert_eq!(
        flat_positions(&ranked_for(&root, &conn, "crop_yield", None)),
        baseline,
        "no ranking change below the gate"
    );

    // 50 events, STILL 1 session: repetition alone never activates.
    for _ in 0..45 {
        record_and_learn(&root, &conn, "crop_yield", Pick::SrcImpl, "only-session", 1000);
    }
    assert!(
        learning::load_learned(&conn, &feedback_config(true), &fixture_weights(), 1000)
            .unwrap()
            .is_none(),
        "50 obs / 1 session stays inert (AR-044)"
    );

    // The same feature across sessions does clear the gate.
    for n in 0..8 {
        record_and_learn(&root, &conn, "crop_yield", Pick::SrcImpl, &format!("multi-{n}"), 1000);
    }
    assert!(
        learning::load_learned(&conn, &feedback_config(true), &fixture_weights(), 1000)
            .unwrap()
            .is_some(),
        "53 obs / 9 sessions clears the gate"
    );
    drop(conn);
    drop(dir);
}

// ---------------------------------------------------------------------------
// AC: a newly recorded feature changes no ranking until evidence arrives
// ---------------------------------------------------------------------------

#[test]
fn new_feature_has_no_influence_until_evidence() {
    let (dir, root) = learning_repo(true, "");
    let conn = open_index(&root);
    for n in 0..12 {
        record_and_learn(&root, &conn, "crop_yield", Pick::SrcImpl, &format!("s{n}"), 1000);
    }
    let learned = learning::load_learned(&conn, &feedback_config(true), &fixture_weights(), 1000)
        .unwrap()
        .expect("gated rows");
    let before = flat_positions(&ranked_for(&root, &conn, "crop_yield", Some(learned.clone())));

    // A brand-new trait file enters the index; its symbol:kind=trait key
    // appears in freshly recorded slates...
    fs::create_dir_all(root.join("src/ledger")).unwrap();
    fs::write(
        root.join("src/ledger/new_trait.rs"),
        "// A brand new abstraction.\npub trait Ledger {\n    fn balance(&self) -> f64;\n}\n",
    )
    .unwrap();
    wonk::pipeline::build_index(&root, true).unwrap();
    let ranked = ranked_for(&root, &conn, "Ledger", None);
    let stored =
        feedback::build_and_store_slate(&conn, "Ledger", &ranked, &feedback_config(true)).unwrap();
    assert!(
        stored.members.iter().any(|m| m.groups
            .symbol
            .get("kind")
            .map(|k| k == "trait")
            .unwrap_or(false)),
        "the trait key entered the recorded set"
    );

    // ...but the never-observed key has no row, so nothing changes: the
    // crop_yield ranking is bit-identical to the pre-trait state.
    assert!(
        learned.evidence()
            .iter()
            .all(|row| !row.feature.starts_with("symbol:kind=trait")),
        "no trait rows were learned"
    );
    let after = flat_positions(&ranked_for(&root, &conn, "crop_yield", Some(learned)));
    assert_eq!(after, before, "unseen features carry no influence");
    drop(conn);
    drop(dir);
}

// ---------------------------------------------------------------------------
// AC: a repository with no feedback ranks exactly as feature-disabled
// ---------------------------------------------------------------------------

#[test]
fn no_feedback_repo_ranks_exactly_as_disabled() {
    // Two identical repos — one with [feedback] enabled (empty store),
    // one disabled. Same search, byte-identical output.
    let (dir_on, root_on) = learning_repo(true, "");
    let (dir_off, root_off) = learning_repo(false, "");

    for root in [&root_on, &root_off] {
        let (code, _, stderr) = run_wonk(root, &["search", "--include-tests", "crop_yield"]);
        assert_eq!(code, 0, "{stderr}");
    }
    let (code_on, out_on, why_on) = run_wonk(&root_on, &["search", "--include-tests", "--why", "crop_yield"]);
    let (code_off, out_off, why_off) = run_wonk(&root_off, &["search", "--include-tests", "--why", "crop_yield"]);
    assert_eq!(code_on, 0, "{why_on}");
    assert_eq!(code_off, 0, "{why_off}");
    assert_eq!(result_lines(&out_on), result_lines(&out_off), "ranking identical");
    assert_eq!(why_on, why_off, "--why breakdown identical");

    // And the below-gate store is equally inert.
    let conn = open_index(&root_on);
    for _ in 0..5 {
        record_and_learn(&root_on, &conn, "crop_yield", Pick::SrcImpl, "one", 1000);
    }
    let (code_below, out_below, why_below) = run_wonk(&root_on, &["search", "--include-tests", "--why", "crop_yield"]);
    assert_eq!(code_below, 0);
    assert_eq!(result_lines(&out_below), result_lines(&out_off), "below-gate ranking identical");
    assert_eq!(why_below, why_off, "below-gate --why identical");
    drop(conn);
    drop(dir_off);
    drop(dir_on);
}

// ---------------------------------------------------------------------------
// Determinism: the same events + the same clock replay identically
// ---------------------------------------------------------------------------

#[test]
fn learning_replays_identically_from_identical_events() {
    let mut dumps = Vec::new();
    for _ in 0..2 {
        // Two IDENTICAL repos: same content, same event sequence, same
        // injected clock — the weights must come out bit-identical.
        let (dir, root) = learning_repo(true, "");
        let conn = open_index(&root);
        for n in 0..10 {
            record_and_learn(&root, &conn, "crop_yield", Pick::SrcImpl, &format!("s{n}"), 5000);
        }
        dumps.push(learned_dump(&conn));
        drop(conn);
        drop(dir);
    }
    assert_eq!(dumps[0], dumps[1], "bit-identical weights on replay");
}

// ---------------------------------------------------------------------------
// The query-path contract: one read, no writes
// ---------------------------------------------------------------------------

static TRACE_SQL: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

fn trace_stmts(event: rusqlite::trace::TraceEvent<'_>) {
    if let rusqlite::trace::TraceEvent::Stmt(_, sql) = event
        && let Ok(mut log) = TRACE_SQL.lock()
    {
        log.push(sql.to_string());
    }
}

#[test]
fn ranked_search_adds_one_learned_read_and_no_writes() {
    let (dir, root) = learning_repo(true, "");
    let conn = open_index(&root);

    // Learn FIRST (outside every trace window): the query path under
    // test is the READ side.
    for n in 0..12 {
        record_and_learn(&root, &conn, "crop_yield", Pick::SrcImpl, &format!("s{n}"), 1000);
    }
    // Baseline: capture-on search with NO learned rows — no
    // learned_weights access at all.
    conn.trace_v2(
        rusqlite::trace::TraceEventCodes::SQLITE_TRACE_STMT,
        Some(trace_stmts),
    );
    let baseline = {
        ranked_for(&root, &conn, "crop_yield", None);
        TRACE_SQL.lock().unwrap().clone()
    };
    assert!(
        baseline.iter().all(|stmt| !stmt.to_lowercase().contains("learned_weights")),
        "no learned rows → no learned read"
    );

    // Gated rows: the load (one learned_weights read) plus the ranked
    // search — beyond the baseline only the pass's chunked symbols
    // reads, and never a write.
    {
        let reloaded = learning::load_learned(&conn, &feedback_config(true), &fixture_weights(), 1000)
            .unwrap()
            .expect("gated rows");
        ranked_for(&root, &conn, "crop_yield", Some(reloaded));
    }
    let statements = TRACE_SQL.lock().unwrap().clone();
    conn.trace_v2(rusqlite::trace::TraceEventCodes::SQLITE_TRACE_STMT, None);

    let learned_reads = statements
        .iter()
        .filter(|s| s.to_lowercase().contains("from learned_weights"))
        .count();
    assert_eq!(learned_reads, 1, "exactly one learned_weights read");
    for stmt in &statements {
        let lowered = stmt.to_lowercase();
        let reads_symbols = lowered.contains("from symbols");
        let reads_learned = lowered.contains("from learned_weights");
        let probes_schema = lowered.contains("from sqlite_master");
        let prepare_shaped = baseline.iter().any(|b| b == stmt);
        assert!(
            reads_symbols || reads_learned || probes_schema || prepare_shaped,
            "statement beyond the contract: {stmt}"
        );
        assert!(
            !lowered.starts_with("insert")
                && !lowered.starts_with("update")
                && !lowered.starts_with("delete")
                && !lowered.contains("feedback_slates")
                && !lowered.contains("into learned"),
            "the query path never writes: {stmt}"
        );
    }
    drop(conn);
    drop(dir);
}

