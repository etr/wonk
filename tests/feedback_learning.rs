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
    stdout
        .lines()
        .filter(|l| !l.starts_with("slate: "))
        .collect()
}

fn feedback_config(enabled: bool) -> FeedbackConfig {
    FeedbackConfig {
        enabled,
        ..FeedbackConfig::default()
    }
}

/// The ranked search the dispatch layer would hold, with capture and the
/// learned overlay both threaded — the `[feedback] enabled` search shape.
fn ranked_for(
    root: &Path,
    conn: &Connection,
    query: &str,
    learned: Option<learning::LearnedTable>,
) -> wonk::rerank::RankedSearch {
    ranked_for_with(root, conn, query, learned, fixture_weights())
}

/// [`ranked_for`] over an explicit weight table — the negative-default
/// repair exercises a demotion-style configured weight.
fn ranked_for_with(
    root: &Path,
    conn: &Connection,
    query: &str,
    learned: Option<learning::LearnedTable>,
    weights: HashMap<String, f32>,
) -> wonk::rerank::RankedSearch {
    let root_str = root.display().to_string();
    let mut results = wonk::search::text_search(query, false, false, &[root_str]).unwrap();
    for result in &mut results {
        if let Ok(rel) = result.file.strip_prefix(root) {
            result.file = rel.to_path_buf();
        }
    }
    let settings = RankSettings {
        use_pipeline: true,
        weights: wonk::rerank::WeightTable::from_config(&weights).unwrap(),
        feedback_capture: true,
        learned,
        ..RankSettings::default()
    };
    wonk::rerank::rank_and_explain_classed(&results, Some(conn), query, &settings)
}

/// Record one useful-identity event from a real search and learn from
/// everything pending. Returns the identity used.
fn record_and_learn(
    root: &Path,
    conn: &Connection,
    query: &str,
    pick: Pick,
    session: &str,
    now: i64,
) -> String {
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
    feedback::record_feedback(
        conn,
        &stored.token,
        std::slice::from_ref(&identity),
        session,
    )
    .unwrap();
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
        &[
            "search",
            "--include-tests",
            "--format",
            "json",
            "crop_yield",
        ],
    );
    assert_eq!(code, 0, "stderr: {stderr}");
    let rows = json_rows(&stdout);
    let src_identity = rows
        .iter()
        .find(|row| row["file"].as_str().unwrap().ends_with("src/crop/mod.rs"))
        .and_then(|row| row["identity"].as_str())
        .expect("identity on rows")
        .to_string();
    let slate = rows[0]["slate"]
        .as_str()
        .expect("slate on rows")
        .to_string();
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
    let (code, _, why_stderr) =
        run_wonk(&root, &["search", "--include-tests", "--why", "crop_yield"]);
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
        record_and_learn(
            &root,
            &conn,
            "crop_yield",
            Pick::SrcImpl,
            &format!("s{n}"),
            1000,
        );
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
        .filter(|item| {
            item.classified
                .result
                .file
                .to_string_lossy()
                .ends_with(suffix)
        })
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
        record_and_learn(
            &root,
            &conn,
            "crop_yield",
            Pick::SrcImpl,
            &format!("s{n}"),
            1000,
        );
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
        record_and_learn(
            &root,
            &conn,
            "granary",
            Pick::Rank(1),
            &format!("late-{n}"),
            2000,
        );
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
            assert!(
                pinned,
                "500 same-direction events pin the bound: {feature} = {weight}"
            );
        } else {
            let default = fixture_weights().get(feature).copied().unwrap_or(0.0);
            let (lo, hi) = (default * 0.5, default * 1.5);
            assert!(
                *weight >= lo - 1e-6 && *weight <= hi + 1e-6,
                "signal within its multiplicative bound: {feature} = {weight} not in [{lo}, {hi}]"
            );
            let pinned = (*weight - hi).abs() < 1e-6 || (*weight - lo).abs() < 1e-6;
            assert!(
                pinned,
                "the bound is REACHED: {feature} = {weight} not at {lo}/{hi}"
            );
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
    let mut groups = wonk::feedback::FeatureGroups {
        signals: vec![
            contribution("path_character", path_character),
            contribution("lexical", 1.0 - path_character),
        ],
        ..wonk::feedback::FeatureGroups::default()
    };
    if path_character > 0.5 {
        groups.path.insert("src".to_string(), "1".to_string());
        groups
            .path
            .insert("class".to_string(), "ordinary".to_string());
    } else {
        groups.path.insert("tests".to_string(), "1".to_string());
        groups.path.insert("class".to_string(), "test".to_string());
    }
    groups
}

// ---------------------------------------------------------------------------
// AC (iter-1 repair): a negative configured signal weight is legal config —
// bounded learning on BOTH the learn and the load/query paths, never a
// panic (PRD-FB-REQ-010)
// ---------------------------------------------------------------------------

#[test]
fn negative_configured_signal_weight_is_bounded_on_both_paths() {
    let (dir, root) = learning_repo(true, "");
    let conn = open_index(&root);

    // A demotion-style negative default for path_character: legal config
    // (WeightTable::from_config rejects only unknown names and
    // non-finite values).
    let weights: HashMap<String, f32> = HashMap::from([
        ("path_character".to_string(), -0.2),
        ("lexical".to_string(), 0.4),
        ("feedback".to_string(), 0.35),
    ]);
    let span = 0.5f32 * 0.2; // dev * |default|
    let (lo, hi) = (-0.2 - span, -0.2 + span);

    // LEARN PATH: 40 events preferring the useful member drive
    // next_weight's clamp around the negative default — the inverted
    // bounds would panic here.
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
        slate: "negative-default".to_string(),
        members: vec![alt, useful],
    };
    let payload = serde_json::to_string(&features).unwrap();
    for id in 1..=40 {
        conn.execute(
            "INSERT INTO feedback_events \
             (id, result_identity, query_class, chosen_rank, features, useful, session, created_at) \
             VALUES (?1, 'u', NULL, 2, ?2, 1, ?3, ?4)",
            rusqlite::params![id, payload, format!("neg{}", id % 4), id],
        )
        .unwrap();
    }
    learning::learn_pending(&conn, &feedback_config(true), &weights, 10_000).unwrap();

    for (feature, _, weight, _, _) in learned_dump(&conn) {
        if feature == "path_character" {
            assert!(
                weight >= lo - 1e-6 && weight <= hi + 1e-6,
                "negative-default signal within its bounds: {weight} not in [{lo}, {hi}]"
            );
        }
    }

    // QUERY PATH: evidence_of's load-time re-clamp over the stored rows
    // (the dispatch's one learned_weights read), then the full ranked
    // search under the negative configured weight — a bounded value, not
    // a process crash.
    let learned = learning::load_learned(&conn, &feedback_config(true), &weights, 10_000)
        .unwrap()
        .expect("40 obs / 4 sessions clear the gates");
    for row in learned.evidence() {
        if row.feature == "path_character" {
            assert!(
                row.effective >= lo - 1e-6 && row.effective <= hi + 1e-6,
                "re-clamped effective {} within [{lo}, {hi}]",
                row.effective
            );
        }
    }
    let ranked = ranked_for_with(&root, &conn, "crop_yield", Some(learned), weights);
    assert!(
        !ranked.groups.is_empty(),
        "the query path completes under a negative configured weight"
    );
    drop(conn);
    drop(dir);
}

// ---------------------------------------------------------------------------
// AC: weights decay toward defaults with age
// ---------------------------------------------------------------------------

#[test]
fn learned_weights_decay_toward_defaults_with_age() {
    let (dir, root) = learning_repo(true, "");
    let conn = open_index(&root);
    for n in 0..12 {
        record_and_learn(
            &root,
            &conn,
            "crop_yield",
            Pick::SrcImpl,
            &format!("s{n}"),
            1000,
        );
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
        record_and_learn(
            &root,
            &conn,
            "crop_yield",
            Pick::SrcImpl,
            "only-session",
            1000,
        );
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
        record_and_learn(
            &root,
            &conn,
            "crop_yield",
            Pick::SrcImpl,
            "only-session",
            1000,
        );
    }
    assert!(
        learning::load_learned(&conn, &feedback_config(true), &fixture_weights(), 1000)
            .unwrap()
            .is_none(),
        "50 obs / 1 session stays inert (AR-044)"
    );

    // The same feature across sessions does clear the gate.
    for n in 0..8 {
        record_and_learn(
            &root,
            &conn,
            "crop_yield",
            Pick::SrcImpl,
            &format!("multi-{n}"),
            1000,
        );
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
        record_and_learn(
            &root,
            &conn,
            "crop_yield",
            Pick::SrcImpl,
            &format!("s{n}"),
            1000,
        );
    }
    let learned = learning::load_learned(&conn, &feedback_config(true), &fixture_weights(), 1000)
        .unwrap()
        .expect("gated rows");
    let before = flat_positions(&ranked_for(
        &root,
        &conn,
        "crop_yield",
        Some(learned.clone()),
    ));

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
        stored.members.iter().any(|m| m
            .groups
            .symbol
            .get("kind")
            .map(|k| k == "trait")
            .unwrap_or(false)),
        "the trait key entered the recorded set"
    );

    // ...but the never-observed key has no row, so nothing changes: the
    // crop_yield ranking is bit-identical to the pre-trait state.
    assert!(
        learned
            .evidence()
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
    let (code_on, out_on, why_on) = run_wonk(
        &root_on,
        &["search", "--include-tests", "--why", "crop_yield"],
    );
    let (code_off, out_off, why_off) = run_wonk(
        &root_off,
        &["search", "--include-tests", "--why", "crop_yield"],
    );
    assert_eq!(code_on, 0, "{why_on}");
    assert_eq!(code_off, 0, "{why_off}");
    assert_eq!(
        result_lines(&out_on),
        result_lines(&out_off),
        "ranking identical"
    );
    assert_eq!(why_on, why_off, "--why breakdown identical");

    // And the below-gate store is equally inert.
    let conn = open_index(&root_on);
    for _ in 0..5 {
        record_and_learn(&root_on, &conn, "crop_yield", Pick::SrcImpl, "one", 1000);
    }
    let (code_below, out_below, why_below) = run_wonk(
        &root_on,
        &["search", "--include-tests", "--why", "crop_yield"],
    );
    assert_eq!(code_below, 0);
    assert_eq!(
        result_lines(&out_below),
        result_lines(&out_off),
        "below-gate ranking identical"
    );
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
            record_and_learn(
                &root,
                &conn,
                "crop_yield",
                Pick::SrcImpl,
                &format!("s{n}"),
                5000,
            );
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
        record_and_learn(
            &root,
            &conn,
            "crop_yield",
            Pick::SrcImpl,
            &format!("s{n}"),
            1000,
        );
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
        baseline.iter().all(|stmt| {
            !stmt.to_lowercase().contains("learned_weights")
                && !stmt.to_lowercase().contains("result_preferences")
        }),
        "no learned rows → no learned read"
    );

    // Gated rows: the load (one learned_weights read AND one
    // result_preferences read — TASK-104) plus the ranked search —
    // beyond the baseline only the pass's chunked symbols reads, and
    // never a write.
    {
        let reloaded =
            learning::load_learned(&conn, &feedback_config(true), &fixture_weights(), 1000)
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
    let preference_reads = statements
        .iter()
        .filter(|s| s.to_lowercase().contains("from result_preferences"))
        .count();
    assert_eq!(
        preference_reads, 1,
        "exactly one result_preferences read — the preference adds one read, no writes"
    );
    for stmt in &statements {
        let lowered = stmt.to_lowercase();
        let reads_symbols = lowered.contains("from symbols");
        let reads_learned = lowered.contains("from learned_weights");
        let reads_preferences = lowered.contains("from result_preferences");
        let probes_schema = lowered.contains("from sqlite_master");
        let prepare_shaped = baseline.iter().any(|b| b == stmt);
        assert!(
            reads_symbols || reads_learned || reads_preferences || probes_schema || prepare_shaped,
            "statement beyond the contract: {stmt}"
        );
        assert!(
            !lowered.starts_with("insert")
                && !lowered.starts_with("update")
                && !lowered.starts_with("delete")
                && !lowered.contains("feedback_slates")
                && !lowered.contains("into learned")
                && !lowered.contains("into result_preferences"),
            "the query path never writes: {stmt}"
        );
    }
    drop(conn);
    drop(dir);
}

// ---------------------------------------------------------------------------
// AC (iter-1 repair): with feedback enabled the candidate feature
// extraction (symbol bulk-load + group extraction + cardinality cap) runs
// ONCE per query, shared between the descriptive pre-sort pass and the
// slate build
// ---------------------------------------------------------------------------

static TRACE_SQL_SHARE: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

fn trace_stmts_share(event: rusqlite::trace::TraceEvent<'_>) {
    if let rusqlite::trace::TraceEvent::Stmt(_, sql) = event
        && let Ok(mut log) = TRACE_SQL_SHARE.lock()
    {
        log.push(sql.to_string());
    }
}

#[test]
fn the_descriptive_pass_and_slate_build_share_one_extraction() {
    let (dir, root) = learning_repo(true, "");
    let conn = open_index(&root);

    // Gated DESCRIPTIVE rows: 12 events whose useful member carries
    // path:src and whose alternative does not — path:src learns, clears
    // the gates, and the pre-sort descriptive pass runs on the next
    // search.
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
        slate: "share".to_string(),
        members: vec![alt, useful],
    };
    let payload = serde_json::to_string(&features).unwrap();
    for id in 1..=12 {
        conn.execute(
            "INSERT INTO feedback_events \
             (id, result_identity, query_class, chosen_rank, features, useful, session, created_at) \
             VALUES (?1, 'u', NULL, 2, ?2, 1, ?3, ?4)",
            rusqlite::params![id, payload, format!("shr{}", id % 4), id],
        )
        .unwrap();
    }
    learning::learn_pending(&conn, &feedback_config(true), &fixture_weights(), 10_000).unwrap();
    let learned = learning::load_learned(&conn, &feedback_config(true), &fixture_weights(), 10_000)
        .unwrap()
        .expect("gated descriptive rows");
    assert!(
        learned
            .evidence()
            .iter()
            .any(|row| row.feature.starts_with("path:")),
        "a descriptive row must be gated for the pass to run"
    );

    // The query: ranked search (the descriptive pass extracts) + the
    // slate build — ONE symbol bulk-load between them.
    conn.trace_v2(
        rusqlite::trace::TraceEventCodes::SQLITE_TRACE_STMT,
        Some(trace_stmts_share),
    );
    let stored = {
        let ranked = ranked_for(&root, &conn, "crop_yield", Some(learned));
        feedback::build_and_store_slate(&conn, "crop_yield", &ranked, &feedback_config(true))
            .unwrap()
    };
    let statements = TRACE_SQL_SHARE.lock().unwrap().clone();
    conn.trace_v2(rusqlite::trace::TraceEventCodes::SQLITE_TRACE_STMT, None);

    let bulk_loads = statements
        .iter()
        .filter(|s| {
            s.to_lowercase()
                .starts_with("select id, file, line, end_line, name, kind, scope, signature, language from symbols where file in")
        })
        .count();
    assert_eq!(
        bulk_loads, 1,
        "the symbol bulk-load runs exactly once per query (pass + slate share it): {statements:?}"
    );
    // The slate still records its descriptive keys.
    assert!(
        stored
            .members
            .iter()
            .any(|m| m.file.ends_with("src/crop/mod.rs") && m.groups.path.contains_key("src")),
        "the shared extraction still feeds the recorded groups"
    );

    // Byte identity: the shared-extraction slate records the same
    // descriptive groups a from-scratch extraction would (signals
    // excluded — the pass contributes a `feedback` row the plain search
    // lacks).
    let plain = ranked_for(&root, &conn, "crop_yield", None);
    let stored_plain =
        feedback::build_and_store_slate(&conn, "crop_yield", &plain, &feedback_config(true))
            .unwrap();
    let descriptive = |stored: &feedback::StoredSlate| {
        stored
            .members
            .iter()
            .map(|m| {
                let mut groups = m.groups.clone();
                groups.signals = Vec::new();
                (m.identity.clone(), groups)
            })
            .collect::<HashMap<String, wonk::feedback::FeatureGroups>>()
    };
    assert_eq!(
        descriptive(&stored),
        descriptive(&stored_plain),
        "shared-extraction slate bytes equal from-scratch extraction bytes"
    );
    drop(conn);
    drop(dir);
}

// ---------------------------------------------------------------------------
// OQ-019: the deterministic tuning experiment (D4). Real usage traces do
// not exist yet, so the honest method is a deterministic simulation over
// synthetic-but-shaped event streams derived from the integration
// fixture's slate shapes, driven by a fixed xorshift per scenario. The
// sweep prints its table under --nocapture; the pinning test below
// asserts the CHOSEN constants' recorded outcomes so
// bench/feedback-learning-tuning.md cannot drift from the code.
// ---------------------------------------------------------------------------

mod tuning {
    use std::collections::HashMap;

    /// Deterministic xorshift — no new dependencies (the D4 contract).
    struct XorShift(u64);

    impl XorShift {
        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x
        }

        /// Uniform in [0, 1).
        fn unit(&mut self) -> f32 {
            (self.next() % 1_000_003) as f32 / 1_000_003.0
        }
    }

    /// The learnable stand-in for the sweep: a `path_character`-shaped
    /// signal, default 0.6, dev 0.5 → bounds [0.3, 0.9]; the deviation
    /// band is 0.3 wide and its 50%-of-bound mark sits at 0.75.
    fn params(step: f32, half_life_days: i64) -> super::learning::LearnParams {
        super::learning::LearnParams::from_config(
            &super::FeedbackConfig {
                learn_step: step,
                learn_half_life_days: half_life_days,
                ..super::feedback_config(true)
            },
            &HashMap::from([("path_character".to_string(), 0.6)]),
        )
    }

    const DEFAULT: f32 = 0.6;
    const HALF_BOUND: f32 = 0.15; // 50% of the 0.3-wide deviation band
    const QUARTER_BOUND: f32 = 0.075;

    /// One scenario's advantage stream: 2000 events, one per simulated
    /// hour, `seed` fixed per scenario name.
    fn advantages(scenario: &str) -> Vec<f32> {
        let seed = match scenario {
            "consistent" => 0x6469736b,
            "noisy" => 0x6e6f6973,
            "adversarial" => 0x61647630,
            "stale" => 0x7374616c,
            "flip" => 0x666c6970,
            _ => unreachable!(),
        };
        let mut rng = XorShift(seed | 1);
        (0..2000)
            .map(|i| match scenario {
                // 85% of events favor the true direction, |A| ≈ 0.8.
                "consistent" => {
                    let sign = if rng.unit() < 0.85 { 1.0 } else { -1.0 };
                    sign * (0.7 + 0.2 * rng.unit())
                }
                // A weak true direction: 60% +0.3 / 40% −0.3 — mean
                // advantage +0.06, the shape of mixed real feedback.
                "noisy" => {
                    let sign = if rng.unit() < 0.60 { 1.0 } else { -1.0 };
                    sign * 0.3
                }
                // 100% one direction at full magnitude.
                "adversarial" => 1.0,
                // The stale burst uses the consistent shape for its 50
                // events; the silence after is handled by the driver.
                "stale" => {
                    let sign = if rng.unit() < 0.85 { 1.0 } else { -1.0 };
                    sign * (0.7 + 0.2 * rng.unit())
                }
                // Consistent, then at event 1000 the true direction
                // reverses.
                "flip" => {
                    let sign = if rng.unit() < 0.85 { 1.0 } else { -1.0 };
                    let direction = if i < 1000 { 1.0 } else { -1.0 };
                    direction * sign * (0.7 + 0.2 * rng.unit())
                }
                _ => unreachable!(),
            })
            .collect()
    }

    /// Drive the update rule over one scenario's stream (hourly events),
    /// recording the weight after every event.
    fn trajectory(scenario: &str, step: f32, half_life: i64) -> Vec<f32> {
        let params = params(step, half_life);
        let stream = advantages(scenario);
        let events = if scenario == "stale" {
            50
        } else {
            stream.len()
        };
        let mut weight = DEFAULT;
        let mut updated_at = 0i64;
        let mut out = Vec::with_capacity(events);
        for (i, advantage) in stream.iter().take(events).enumerate() {
            let now = (i as i64) * 3600;
            weight = super::learning::next_weight(
                weight,
                updated_at,
                now,
                *advantage,
                "path_character",
                &params,
            );
            updated_at = now;
            out.push(weight);
        }
        out
    }

    /// The five metrics of one (step, half-life) cell:
    /// (a) events to 50% of the bound (consistent stream; 2000 = never),
    /// (b) noisy-stream residual |effective − default| after 2000 events,
    /// (c) sign flips of effective − default in the noisy stream,
    /// (d) days for the stale stream to fall back under 25% of bound,
    /// (e) bound violations across every stream (must be 0).
    fn metrics(step: f32, half_life: i64) -> (usize, f32, usize, f32, usize) {
        let consistent = trajectory("consistent", step, half_life);
        let noisy = trajectory("noisy", step, half_life);
        let adversarial = trajectory("adversarial", step, half_life);
        let stale = trajectory("stale", step, half_life);

        let a = consistent
            .iter()
            .position(|w| (w - DEFAULT).abs() >= HALF_BOUND)
            .map(|i| i + 1)
            .unwrap_or(2000);
        let b = (noisy[noisy.len() - 1] - DEFAULT).abs();
        let c = noisy
            .windows(2)
            .filter(|pair| {
                let (x, y) = (pair[0] - DEFAULT, pair[1] - DEFAULT);
                x * y < 0.0
            })
            .count();
        let d = {
            // After the 50-event burst the row ages undisturbed; find
            // the days until the read-time decayed deviation falls under
            // 25% of the bound.
            let stored = stale[stale.len() - 1];
            let deviation = (stored - DEFAULT).abs().max(1e-9);
            half_life as f32 * (deviation / QUARTER_BOUND).log2().max(0.0)
        };
        let e = [consistent, noisy, adversarial, stale]
            .iter()
            .flat_map(|t| t.iter())
            .filter(|w| **w < 0.3 - 1e-6 || **w > 0.9 + 1e-6)
            .count();
        (a, b, c, d, e)
    }

    #[test]
    fn sweep_prints_the_grid_table() {
        println!("step | half_life |  a  |   b   | c |    d    | e");
        for step in [0.01f32, 0.02, 0.05] {
            for half_life in [14i64, 30, 60] {
                let (a, b, c, d, e) = metrics(step, half_life);
                println!("{step:.2} | {half_life:9} | {a:3} | {b:.3} | {c} | {d:7.1} | {e}");
            }
        }
        // (e) is the adversarial guarantee: it must hold for EVERY cell.
        for step in [0.01f32, 0.02, 0.05] {
            for half_life in [14i64, 30, 60] {
                let (_, _, _, _, e) = metrics(step, half_life);
                assert_eq!(e, 0, "step {step} hl {half_life} violated the bound");
            }
        }
    }

    /// The pinning test: the CHOSEN constants (step 0.02, half-life 30d —
    /// the `[feedback]` defaults, retained by the grid; see
    /// bench/feedback-learning-tuning.md) and their recorded outcomes,
    /// verbatim, so the bench doc cannot drift from the code.
    ///
    /// The plan's aspirational wants for (b) residual and (c) flips are
    /// UNSATISFIABLE at the hourly event rate for EVERY grid cell: i.i.d.
    /// evidence with any drift saturates the band, and the clamp — metric
    /// (e), zero everywhere — is precisely the property that makes that
    /// safe. The grid therefore separates cells only on (a); every step
    /// already meets the "tens of events" want, and the defaults keep the
    /// mildest step that converges quickly.
    #[test]
    fn chosen_constants_pin_their_recorded_outcomes() {
        let (a, b, c, d, e) = metrics(0.02, 30);
        assert_eq!(a, 12, "events to 50% of the bound (consistent)");
        assert!((b - 0.286).abs() < 0.005, "noisy residual: {b}");
        assert_eq!(c, 23, "noisy-stream sign flips");
        assert!((d - 60.0).abs() < 0.1, "stale falls back in days: {d}");
        assert_eq!(e, 0, "no bound violations");
    }
}

// ---------------------------------------------------------------------------
// TASK-103 AC1: --no-feedback reproduces index-only ranking exactly
// ---------------------------------------------------------------------------

/// Run `wonk search --include-tests --why PATTERN` with an optional
/// `--no-feedback`, returning (code, result stdout, why stderr).
fn search_with_why(root: &Path, no_feedback: bool, query: &str) -> (i32, String, String) {
    let mut args = vec!["search", "--include-tests", "--why"];
    if no_feedback {
        args.push("--no-feedback");
    }
    args.push(query);
    let (code, stdout, stderr) = run_wonk(root, &args);
    (code, result_lines(&stdout).join("\n"), stderr)
}

#[test]
fn no_feedback_reproduces_index_only_ranking_exactly() {
    let (dir, root) = learning_repo(true, "");

    // Baseline BEFORE any feedback: index-only ranking, why totals and
    // all. (Slates record on every enabled search — including
    // --no-feedback: capture is not influence.)
    let (_, base_stdout, base_why) = search_with_why(&root, false, "crop_yield");

    // Learn to a gated, order-shifting state: 40 events preferring the
    // src implementation across 40 sessions (the
    // preferring_implementation_shifts_path_character_visibly pattern).
    let (code, stdout, stderr) = run_wonk(
        &root,
        &[
            "search",
            "--include-tests",
            "--format",
            "json",
            "crop_yield",
        ],
    );
    assert_eq!(code, 0, "stderr: {stderr}");
    let rows = json_rows(&stdout);
    let src_identity = rows
        .iter()
        .find(|row| row["file"].as_str().unwrap().ends_with("src/crop/mod.rs"))
        .and_then(|row| row["identity"].as_str())
        .expect("identity on rows")
        .to_string();
    let slate = rows[0]["slate"]
        .as_str()
        .expect("slate on rows")
        .to_string();
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

    // Sanity: the overlay is LIVE — the learned search differs from the
    // baseline (proves the reproduction below is doing real work).
    let (_, learned_stdout, _) = search_with_why(&root, false, "crop_yield");
    assert_ne!(
        learned_stdout, base_stdout,
        "the learned overlay must shift the enabled search"
    );

    // THE assertion: --no-feedback reproduces the pre-learning baseline
    // byte-for-byte — results and why lines (weights restored to their
    // defaults, no descriptive feedback rows, no learned: line).
    let (code, free_stdout, free_why) = search_with_why(&root, true, "crop_yield");
    assert_eq!(code, 0);
    assert_eq!(free_stdout, base_stdout, "result lines identical");
    assert_eq!(free_why, base_why, "why stderr identical");
    assert!(
        !free_why.contains("learned: "),
        "no learned line without the overlay: {free_why}"
    );

    // Library twin on the same index: an ATTACHED learned table with
    // feedback_free must equal learned: None at the ScoredResult level.
    let conn = open_index(&root);
    let learned =
        learning::load_learned(&conn, &feedback_config(true), &fixture_weights(), 100_000)
            .unwrap()
            .expect("gated rows exist");
    let with_overlay = ranked_for(&root, &conn, "crop_yield", Some(learned));
    let free = {
        let table =
            learning::load_learned(&conn, &feedback_config(true), &fixture_weights(), 100_000)
                .unwrap()
                .expect("gated rows exist");
        let root_str = root.display().to_string();
        let mut results =
            wonk::search::text_search("crop_yield", false, false, &[root_str]).unwrap();
        for result in &mut results {
            if let Ok(rel) = result.file.strip_prefix(&root) {
                result.file = rel.to_path_buf();
            }
        }
        let settings = RankSettings {
            use_pipeline: true,
            weights: wonk::rerank::WeightTable::from_config(&fixture_weights()).unwrap(),
            feedback_capture: true,
            learned: Some(table),
            feedback_free: true,
            ..RankSettings::default()
        };
        wonk::rerank::rank_and_explain_classed(&results, Some(&conn), "crop_yield", &settings)
    };
    let overlay_bits = flat_scored_bits(&with_overlay);
    let free_bits = flat_scored_bits(&free);
    assert_eq!(overlay_bits.len(), free_bits.len());
    assert_ne!(
        overlay_bits, free_bits,
        "the attached overlay shifts scores when not stripped"
    );
    let none = ranked_for(&root, &conn, "crop_yield", None);
    assert_eq!(
        free_bits,
        flat_scored_bits(&none),
        "feedback_free == learned: None at the bit level"
    );
    drop(conn);
    drop(dir);
}

/// Bit-level comparison key of a ranked search's scored results.
fn flat_scored_bits(ranked: &wonk::rerank::RankedSearch) -> Vec<(String, u64, u32, u32)> {
    ranked
        .groups
        .iter()
        .flat_map(|(_, group)| group.iter())
        .map(|item| {
            (
                item.classified.result.file.to_string_lossy().into_owned(),
                item.classified.result.line,
                item.score.to_bits(),
                item.contributions.len() as u32,
            )
        })
        .collect()
}

/// TASK-103: management modes bypass the `[feedback] enabled` gate —
/// inspecting and wiping leftover state after opting out is exactly
/// when they matter. Recording keeps the gate.
#[test]
fn feedback_management_modes_work_with_feedback_disabled() {
    let (dir, root) = learning_repo(false, "");
    // Leftover state from an opted-out-later life: gated learned rows
    // and recorded events, seeded straight into the index. The weight's
    // updated_at is NOW — the display clock is the real system time
    // here, and a stale stamp would decay the row to its default.
    let conn = open_index(&root);
    let now: i64 = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    conn.execute(
        "INSERT INTO learned_weights \
         (feature, query_class, weight, observations, sessions, updated_at) \
         VALUES ('path_character', '', 0.15, 40, 40, ?1)",
        [now],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO feedback_events \
         (result_identity, query_class, chosen_rank, features, useful, session, created_at) \
         VALUES ('x', 'symbol', 2, ?, 1, 's1', 1000)",
        [r#"{"schema":1,"slate":"t","members":[]}"#],
    )
    .unwrap();
    drop(conn);

    let (code, out, err) = run_wonk(&root, &["feedback", "--weights"]);
    assert_eq!(code, 0, "{err}");
    assert!(
        out.contains("path_character") && out.contains("0.150"),
        "weights display with the feature off: {out}"
    );
    let (code, out, err) = run_wonk(&root, &["feedback", "--list"]);
    assert_eq!(code, 0, "{err}");
    assert!(
        out.contains("s1"),
        "events list with the feature off: {out}"
    );
    let (code, out, err) = run_wonk(&root, &["feedback", "--export"]);
    assert_eq!(code, 0, "{err}");
    assert!(
        serde_json::from_str::<Vec<Value>>(&out).is_ok(),
        "export parses"
    );
    let (code, _, err) = run_wonk(&root, &["feedback", "--reset-weights"]);
    assert_eq!(code, 0, "{err}");
    let (code, _, err) = run_wonk(&root, &["feedback", "--clear-events"]);
    assert_eq!(code, 0, "{err}");

    // Recording still requires the feature on.
    let (code, _, err) = run_wonk(
        &root,
        &[
            "feedback",
            "--slate",
            "t",
            "--session",
            "s",
            "--useful",
            "1",
        ],
    );
    assert_ne!(code, 0);
    assert!(
        err.contains("feedback capture is disabled"),
        "the recording gate stands: {err}"
    );
    drop(dir);
}

// ---------------------------------------------------------------------------
// TASK-103 AC3: weights reset independently of event history
// ---------------------------------------------------------------------------

/// Drive the fixture to the gated 40-event learned state via the real
/// binary; returns (baseline stdout, baseline why, src identity, slate).
fn learn_gated_state(root: &Path) -> (String, String, String, String) {
    let (_, base_stdout, base_why) = search_with_why(root, false, "crop_yield");
    let (code, stdout, stderr) = run_wonk(
        root,
        &[
            "search",
            "--include-tests",
            "--format",
            "json",
            "crop_yield",
        ],
    );
    assert_eq!(code, 0, "stderr: {stderr}");
    let rows = json_rows(&stdout);
    let src_identity = rows
        .iter()
        .find(|row| row["file"].as_str().unwrap().ends_with("src/crop/mod.rs"))
        .and_then(|row| row["identity"].as_str())
        .expect("identity on rows")
        .to_string();
    let slate = rows[0]["slate"]
        .as_str()
        .expect("slate on rows")
        .to_string();
    for n in 0..40 {
        let (code, _, err) = run_wonk(
            root,
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
    (base_stdout, base_why, src_identity, slate)
}

#[test]
fn weights_reset_independently_of_event_history() {
    let (dir, root) = learning_repo(true, "");
    let (base_stdout, base_why, _identity, _slate) = learn_gated_state(&root);

    // Reset ALL weights: confirmation names the independence.
    let (code, out, err) = run_wonk(&root, &["feedback", "--reset-weights"]);
    assert_eq!(code, 0, "{err}");
    assert!(
        out.contains("reset") && out.contains("event history untouched"),
        "the confirmation: {out}"
    );

    // No learned rows remain...
    let (code, out, err) = run_wonk(&root, &["feedback", "--weights"]);
    assert_eq!(code, 0, "{err}");
    assert!(
        !out.contains("path_character"),
        "the learned signal is gone: {out}"
    );

    // ...but the event history still has all 40 events.
    let (code, out, err) = run_wonk(&root, &["feedback", "--export"]);
    assert_eq!(code, 0, "{err}");
    let exported: Vec<Value> = serde_json::from_str(&out).unwrap();
    assert_eq!(exported.len(), 40, "the history stands: {out}");

    // And the search is the baseline again — influence gone, recording
    // still on (the slate line is stripped by search_with_why).
    let (code, stdout, why) = search_with_why(&root, false, "crop_yield");
    assert_eq!(code, 0);
    assert_eq!(stdout, base_stdout, "result lines back to baseline");
    assert_eq!(why, base_why, "why stderr back to baseline");
    drop(dir);
}

#[test]
fn reset_weight_scopes_to_one_feature_leaving_siblings() {
    let (dir, root) = learning_repo(true, "");
    let (_base, _why, _identity, _slate) = learn_gated_state(&root);

    let (code, out, err) = run_wonk(&root, &["feedback", "--weights"]);
    assert_eq!(code, 0, "{err}");
    let before: Vec<&str> = out.lines().collect();
    assert!(before.len() > 1, "several features learned: {out}");

    let (code, out, err) = run_wonk(&root, &["feedback", "--reset-weight", "path_character"]);
    assert_eq!(code, 0, "{err}");
    assert!(
        out.contains("reset") && out.contains("path_character"),
        "per-feature confirmation: {out}"
    );
    let (code, out, err) = run_wonk(&root, &["feedback", "--weights"]);
    assert_eq!(code, 0, "{err}");
    assert!(
        !out.contains("path_character"),
        "the feature is reset: {out}"
    );
    assert!(
        out.lines().count() > 0 && out.contains("obs"),
        "sibling rows stand: {out}"
    );
    drop(dir);
}

#[test]
fn clear_events_leaves_learned_weights_alone() {
    let (dir, root) = learning_repo(true, "");
    let (_base, _why, _identity, _slate) = learn_gated_state(&root);

    // The reverse direction of the independence: wiping history must
    // not touch the learned weights.
    let (code, out, err) = run_wonk(&root, &["feedback", "--clear-events"]);
    assert_eq!(code, 0, "{err}");
    assert!(
        out.contains("cleared 40 feedback event(s); learned weights untouched"),
        "the confirmation: {out}"
    );
    let (code, out, err) = run_wonk(&root, &["feedback", "--weights"]);
    assert_eq!(code, 0, "{err}");
    assert!(
        out.contains("path_character") && out.contains("40 obs, 40 sessions"),
        "learned weights survive the history wipe: {out}"
    );
    drop(dir);
}

// ---------------------------------------------------------------------------
// TASK-103 AC4: list, export, wipe whole and per result
// ---------------------------------------------------------------------------

#[test]
fn feedback_listed_exported_and_wiped_whole_and_per_result() {
    let (dir, root) = learning_repo(true, "");
    // Events against two DIFFERENT results: the src twin and the tests
    // twin of one slate.
    let (code, stdout, stderr) = run_wonk(
        &root,
        &[
            "search",
            "--include-tests",
            "--format",
            "json",
            "crop_yield",
        ],
    );
    assert_eq!(code, 0, "stderr: {stderr}");
    let rows = json_rows(&stdout);
    let pick_identity = |suffix: &str| {
        rows.iter()
            .find(|row| row["file"].as_str().unwrap().ends_with(suffix))
            .and_then(|row| row["identity"].as_str())
            .expect("identity on rows")
            .to_string()
    };
    let src_identity = pick_identity("src/crop/mod.rs");
    let tests_identity = pick_identity("tests/crop_test.rs");
    let slate = rows[0]["slate"].as_str().expect("slate").to_string();
    for (identity, session) in [
        (&src_identity, "sess-a"),
        (&tests_identity, "sess-b"),
        (&src_identity, "sess-c"),
    ] {
        let (code, _, err) = run_wonk(
            &root,
            &[
                "feedback",
                "--slate",
                &slate,
                "--useful",
                identity,
                "--session",
                session,
            ],
        );
        assert_eq!(code, 0, "{err}");
    }

    // --list: one line per event with session and class.
    let (code, out, err) = run_wonk(&root, &["feedback", "--list"]);
    assert_eq!(code, 0, "{err}");
    // grep mode appends one shell-completing newline; count real lines.
    let lines: Vec<&str> = out.lines().filter(|l| !l.is_empty()).collect();
    assert_eq!(lines.len(), 3, "one line per event: {out}");
    assert!(out.contains("sess-a") && out.contains("sess-b"), "{out}");
    assert!(out.contains("class symbol"), "{out}");

    // --export round-trips the store.
    let (code, out, err) = run_wonk(&root, &["feedback", "--export"]);
    assert_eq!(code, 0, "{err}");
    let exported: Vec<Value> = serde_json::from_str(&out).unwrap();
    assert_eq!(exported.len(), 3);

    // --clear-result wipes exactly one result's events.
    let (code, out, err) = run_wonk(&root, &["feedback", "--clear-result", &tests_identity]);
    assert_eq!(code, 0, "{err}");
    assert!(
        out.contains(&format!("cleared 1 feedback event(s) for {tests_identity}")),
        "per-result confirmation: {out}"
    );
    let (code, out, err) = run_wonk(&root, &["feedback", "--list"]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(
        out.lines().filter(|l| !l.is_empty()).count(),
        2,
        "the other result stands: {out}"
    );
    let (code, out, err) = run_wonk(&root, &["feedback", "--export"]);
    assert_eq!(code, 0, "{err}");
    let exported: Vec<Value> = serde_json::from_str(&out).unwrap();
    assert_eq!(exported.len(), 2);

    // --clear-events empties the store.
    let (code, out, err) = run_wonk(&root, &["feedback", "--clear-events"]);
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("cleared 2 feedback event(s)"), "{out}");
    let (code, out, err) = run_wonk(&root, &["feedback", "--list"]);
    assert_eq!(code, 0, "{err}");
    assert!(out.trim().is_empty(), "nothing left: {out}");
    drop(dir);
}

// ---------------------------------------------------------------------------
// TASK-103 AC5: status shows feedback state including deviation
// ---------------------------------------------------------------------------

#[test]
fn status_shows_feedback_state_with_deviation() {
    let (dir, root) = learning_repo(true, "");
    let (_base, _why, _identity, _slate) = learn_gated_state(&root);

    // The deviation is the MAX over gated rows: the 40-event learn
    // saturates descriptive keys at their ±0.5 bound (path_character
    // itself sits at 0.150 vs default 0.100) — the line reads 0.500.
    let (code, _, stderr) = run_wonk(&root, &["status"]);
    assert_eq!(code, 0);
    assert!(
        stderr.contains("Feedback: enabled, 40 events, 40 sessions, weight deviation 0.500"),
        "the feedback line: {stderr}"
    );

    // JSON carries the same state.
    let (code, stdout, stderr) = run_wonk(&root, &["status", "--format", "json"]);
    assert_eq!(code, 0, "{stderr}");
    let status: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(status["feedback"]["enabled"], true);
    assert_eq!(status["feedback"]["events"], 40);
    assert_eq!(status["feedback"]["sessions"], 40);
    assert!(
        (status["feedback"]["deviation"].as_f64().unwrap() - 0.5).abs() < 0.001,
        "deviation reads through: {status}"
    );

    // After a reset: deviation 0.000, events still counted.
    let (code, _, err) = run_wonk(&root, &["feedback", "--reset-weights"]);
    assert_eq!(code, 0, "{err}");
    let (code, _, stderr) = run_wonk(&root, &["status"]);
    assert_eq!(code, 0);
    assert!(
        stderr.contains("Feedback: enabled, 40 events, 40 sessions, weight deviation 0.000"),
        "reset reads through status: {stderr}"
    );
    drop(dir);
}

// ---------------------------------------------------------------------------
// TASK-104: session-gated per-result preferences (PRD-FB-REQ-016, AR-036)
// ---------------------------------------------------------------------------

/// A `now` the CLI's own `system_secs()` clock can see without decay —
/// `record_and_learn`-grown state must survive the real dispatch's load.
fn recent_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

/// Every stored preference row: (identity, strength, observations,
/// sessions, updated_at).
fn preference_dump(conn: &Connection) -> Vec<(String, f32, i64, i64, i64)> {
    let mut stmt = conn
        .prepare(
            "SELECT result_identity, strength, observations, sessions, updated_at \
             FROM result_preferences ORDER BY result_identity",
        )
        .unwrap();
    stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, f32>(1)?,
            row.get::<_, i64>(2)?,
            row.get::<_, i64>(3)?,
            row.get::<_, i64>(4)?,
        ))
    })
    .unwrap()
    .collect::<rusqlite::Result<Vec<_>>>()
    .unwrap()
}

/// Load the gated learned table for the fixture's config and weights.
fn load_table(conn: &Connection, now: i64) -> Option<learning::LearnedTable> {
    learning::load_learned(conn, &feedback_config(true), &fixture_weights(), now).unwrap()
}

#[test]
fn preference_activates_only_across_distinct_sessions() {
    let (dir, root) = learning_repo(true, "");
    let conn = open_index(&root);
    let now = recent_now();

    // Two distinct sessions: below the default gate of 3 — the preference
    // exists in the tables but must not influence anything.
    let identity = record_and_learn(&root, &conn, "crop_yield", Pick::SrcImpl, "s1", now);
    record_and_learn(&root, &conn, "crop_yield", Pick::SrcImpl, "s2", now);
    assert_eq!(
        learning::preference_count(&conn),
        1,
        "the row grows from the first confirming event"
    );
    assert!(
        load_table(&conn, now).is_none(),
        "two sessions gate the preference out entirely"
    );
    let baseline_src = max_score_of(
        &ranked_for(&root, &conn, "crop_yield", None),
        "src/crop/mod.rs",
    )
    .expect("src twin scored");

    // The THIRD distinct session activates it: strength 0.3 at the default
    // gate, riding the feedback weight 0.35.
    record_and_learn(&root, &conn, "crop_yield", Pick::SrcImpl, "s3", now);
    let table = load_table(&conn, now).expect("the preference surfaces the table");
    assert_eq!(
        table.preferences().get(&identity),
        Some(&0.3),
        "one step per distinct session: {:?}",
        table.preferences()
    );
    let preferred = ranked_for(&root, &conn, "crop_yield", Some(table));
    let preferred_src = max_score_of(&preferred, "src/crop/mod.rs").unwrap();
    assert!(
        (preferred_src - baseline_src - 0.3 * 0.35).abs() < 1e-4,
        "the preference joins the score at strength × feedback weight: \
         {preferred_src} vs {baseline_src}"
    );

    // The visible surface: the src twin's why line carries its own
    // preference entry with the exact weighted value.
    let (code, _, why) = run_wonk(&root, &["search", "--include-tests", "--why", "crop_yield"]);
    assert_eq!(code, 0);
    let src_line = why
        .lines()
        .find(|l| l.starts_with("why: ") && l.contains("src/crop/mod.rs"))
        .expect("src why line");
    assert!(
        src_line.contains("preference 0.300*0.35=0.1050"),
        "own contribution row: {src_line}"
    );
    drop(conn);
    drop(dir);
}

#[test]
fn one_session_repetition_never_activates_a_preference() {
    let (dir, root) = learning_repo(true, "");
    let conn = open_index(&root);
    let now = recent_now();

    // AR-036 adversarial: fifty confirming events, ONE session.
    for n in 0..50 {
        record_and_learn(&root, &conn, "crop_yield", Pick::SrcImpl, "solo", now + n);
    }
    let rows = preference_dump(&conn);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].1, 0.1, "one step only: {rows:?}");
    assert_eq!(rows[0].3, 1, "one session only: {rows:?}");
    assert_eq!(rows[0].2, 50, "every event is still evidence");

    // No influence in any direction: the load is empty and the why lines
    // carry no preference entry.
    assert!(
        load_table(&conn, now + 100).is_none(),
        "repetition in one session never activates"
    );
    let (code, _, why) = run_wonk(&root, &["search", "--include-tests", "--why", "crop_yield"]);
    assert_eq!(code, 0);
    assert!(
        !why.contains("preference"),
        "no preference contribution anywhere: {why}"
    );
    drop(conn);
    drop(dir);
}

#[test]
fn preference_row_is_distinct_from_learned_weights_in_why() {
    let (dir, root) = learning_repo(true, "");
    let conn = open_index(&root);
    let now = recent_now();

    // Enough mass to gate BOTH channels: 12 events preferring the src
    // twin grow its preference AND gated descriptive weights (path:* keys
    // the alternatives lack).
    for n in 0..12 {
        record_and_learn(
            &root,
            &conn,
            "crop_yield",
            Pick::SrcImpl,
            &format!("s{n}"),
            now,
        );
    }

    // Text why: the src twin's line shows BOTH entries, separately named.
    let (_, _, why) = search_with_why(&root, false, "crop_yield");
    let src_line = why
        .lines()
        .find(|l| l.starts_with("why: ") && l.contains("src/crop/mod.rs"))
        .expect("src why line");
    assert!(
        src_line.contains("feedback "),
        "the learned row: {src_line}"
    );
    assert!(
        src_line.contains("preference "),
        "the per-result row: {src_line}"
    );

    // The learned: line stays weights-only.
    let learned_line = why
        .lines()
        .find(|l| l.starts_with("learned: "))
        .expect("the learned line prints");
    assert!(
        !learned_line.contains("preference"),
        "preferences never ride the weights line: {learned_line}"
    );

    // JSON: signals[] carries both signal names.
    let (code, stdout, stderr) = run_wonk(
        &root,
        &[
            "search",
            "--include-tests",
            "--why",
            "--format",
            "json",
            "crop_yield",
        ],
    );
    assert_eq!(code, 0, "{stderr}");
    let src_row = json_rows(&stdout)
        .into_iter()
        .find(|row| row["file"].as_str().unwrap().ends_with("src/crop/mod.rs"))
        .expect("src row");
    let names: Vec<&str> = src_row["why"]["signals"]
        .as_array()
        .expect("why signals")
        .iter()
        .map(|signal| signal["signal"].as_str().unwrap())
        .collect();
    assert!(
        names.contains(&"feedback") && names.contains(&"preference"),
        "both channels in JSON signals: {names:?}"
    );
    drop(conn);
    drop(dir);
}

#[test]
fn preference_cannot_outweigh_learned_weights() {
    let (dir, root) = learning_repo(true, "");
    let conn = open_index(&root);
    let now = recent_now();

    // A SATURATED preference for the src twin: six distinct sessions.
    for n in 0..6 {
        record_and_learn(
            &root,
            &conn,
            "crop_yield",
            Pick::SrcImpl,
            &format!("s{n}"),
            now,
        );
    }
    // The tests twin favored by gated descriptive keys summing to the
    // descriptive channel's full 1.0 value clamp (upsert over whatever
    // the six events already observed there): its `tests` ancestor and
    // its Test match category — both B-exclusive.
    for feature in ["path:tests", "match:category=test"] {
        conn.execute(
            "INSERT INTO learned_weights \
             (feature, query_class, weight, observations, sessions, updated_at) \
             VALUES (?1, '', 0.5, 40, 9, ?2) \
             ON CONFLICT(feature, query_class) DO UPDATE SET \
                 weight = 0.5, observations = 40, sessions = 9, updated_at = ?2",
            rusqlite::params![feature, now],
        )
        .unwrap();
    }

    let table = load_table(&conn, now).expect("both channels gated");
    let ranked = ranked_for(&root, &conn, "crop_yield", Some(table));
    let find = |suffix: &str, signal: &str| {
        ranked
            .groups
            .iter()
            .flat_map(|(_, g)| g.iter())
            .find(|s| s.classified.result.file.ends_with(suffix))
            .unwrap()
            .contributions
            .iter()
            .find(|c| c.signal == signal)
            .unwrap()
            .weighted
    };
    let a_preference = find("src/crop/mod.rs", "preference");
    let b_feedback = find("tests/crop_test.rs", "feedback");
    assert!(
        (a_preference - 0.5 * 0.35).abs() < 1e-4,
        "the preference's own ceiling: {a_preference}"
    );
    assert!(
        (b_feedback - 1.0 * 0.35).abs() < 1e-4,
        "the descriptive channel's full clamp: {b_feedback}"
    );
    assert!(
        b_feedback > a_preference,
        "a saturated preference stays under the learned channel: \
         {b_feedback} vs {a_preference}"
    );
    drop(conn);
    drop(dir);
}

#[test]
fn preference_strength_is_capped_under_adversarial_confirmation() {
    let (dir, root) = learning_repo(true, "");
    let conn = open_index(&root);
    let now = recent_now();

    // Twenty distinct sessions all confirming the same result.
    for n in 0..20 {
        record_and_learn(
            &root,
            &conn,
            "crop_yield",
            Pick::SrcImpl,
            &format!("s{n}"),
            now,
        );
    }
    let rows = preference_dump(&conn);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].1, 0.5, "saturated at the cap: {rows:?}");
    assert_eq!(rows[0].3, 20, "sessions keep counting as evidence");

    let table = load_table(&conn, now).expect("gated");
    let ranked = ranked_for(&root, &conn, "crop_yield", Some(table));
    let preference = ranked
        .groups
        .iter()
        .flat_map(|(_, g)| g.iter())
        .find(|s| s.classified.result.file.ends_with("src/crop/mod.rs"))
        .unwrap()
        .contributions
        .iter()
        .find(|c| c.signal == "preference")
        .unwrap();
    assert_eq!(preference.value, 0.5);
    assert!(
        preference.weighted <= 0.5 * 0.35 + 1e-6,
        "never above half the feedback weight's full swing: {}",
        preference.weighted
    );
    drop(conn);
    drop(dir);
}

#[test]
fn preference_decays_and_retires_like_every_entry() {
    let (dir, root) = learning_repo(true, "");
    let conn = open_index(&root);

    // Confirmed across 3 sessions at t=1000: strength 0.3.
    for session in ["a", "b", "c"] {
        record_and_learn(&root, &conn, "crop_yield", Pick::SrcImpl, session, 1000);
    }
    let table = load_table(&conn, 1000).unwrap();
    assert_eq!(table.preferences().len(), 1);

    // One half-life later: the strength halves (PRD-FB-REQ-011 parity).
    let table = load_table(&conn, 1000 + 30 * 86_400).expect("still above the retire floor");
    let effective = *table.preferences().values().next().unwrap();
    assert!(
        (effective - 0.15).abs() < 1e-6,
        "one half-life halves the strength: {effective}"
    );

    // A year unconfirmed: below the floor — excluded at load, and the
    // next learn pass sweeps the row.
    let year = 1000 + 365 * 86_400;
    assert!(
        load_table(&conn, year).is_none(),
        "below the floor means no influence"
    );
    learning::learn_pending(&conn, &feedback_config(true), &fixture_weights(), year).unwrap();
    assert_eq!(
        learning::preference_count(&conn),
        0,
        "the sweep collected the retired row"
    );
    drop(conn);
    drop(dir);
}

#[test]
fn materially_changed_result_loses_its_preference() {
    let (dir, root) = learning_repo(true, "");
    let conn = open_index(&root);
    let now = recent_now();

    for session in ["a", "b", "c"] {
        record_and_learn(&root, &conn, "crop_yield", Pick::SrcImpl, session, now);
    }
    assert_eq!(learning::preference_count(&conn), 1);

    // Material change (PRD-FB-REQ-006): the signature edit re-anchors the
    // symbol's identity — the stored preference can never match again.
    let changed = CROP_IMPL.replace(
        "pub fn crop_yield(acres: f64, rain: f64) -> f64 {",
        "pub fn crop_yield(acres: f64, rain: f64, season: u32) -> f64 {",
    );
    assert_ne!(&changed, CROP_IMPL, "the fixture edit must apply");
    fs::write(root.join("src/crop/mod.rs"), changed).unwrap();
    wonk::pipeline::build_index(&root, true).unwrap();

    // The row is still stored (it is history), but the recomputed identity
    // no longer matches: zero contribution, bit-identical index-only score.
    let table = load_table(&conn, now).expect("the stale row still loads");
    assert_eq!(
        table.preferences().len(),
        1,
        "the row itself is retired at match time"
    );
    let ranked = ranked_for(&root, &conn, "crop_yield", Some(table));
    let baseline = ranked_for(&root, &conn, "crop_yield", None);
    let src = ranked
        .groups
        .iter()
        .flat_map(|(_, g)| g.iter())
        .find(|s| s.classified.result.file.ends_with("src/crop/mod.rs"))
        .unwrap();
    let preference = src
        .contributions
        .iter()
        .find(|c| c.signal == "preference")
        .expect("the channel runs; the match decides");
    assert_eq!(preference.value, 0.0, "identity mismatch → no influence");
    let base_src = baseline
        .groups
        .iter()
        .flat_map(|(_, g)| g.iter())
        .find(|s| s.classified.result.file.ends_with("src/crop/mod.rs"))
        .unwrap();
    assert_eq!(
        src.score.to_bits(),
        base_src.score.to_bits(),
        "the changed result ranks at its index-only score"
    );
    drop(conn);
    drop(dir);
}

#[test]
fn rank_one_confirmations_never_count_toward_a_preference() {
    let (dir, root) = learning_repo(true, "");
    let conn = open_index(&root);
    let now = recent_now();

    // PRD-FB-REQ-009 interplay: picking the rank-1 result across four
    // sessions — a confirmation that never qualifies.
    for session in ["a", "b", "c", "d"] {
        record_and_learn(&root, &conn, "crop_yield", Pick::Rank(1), session, now);
    }
    assert_eq!(
        learning::preference_count(&conn),
        0,
        "rank-1 confirmations never form a preference"
    );
    assert!(
        load_table(&conn, now).is_none(),
        "no learned influence at all"
    );
    drop(conn);
    drop(dir);
}

#[test]
fn no_feedback_reproduces_index_only_ranking_with_preferences() {
    let (dir, root) = learning_repo(true, "");

    // Baseline before any feedback.
    let (_, base_stdout, base_why) = search_with_why(&root, false, "crop_yield");

    // An ACTIVE preference: three distinct sessions (weights stay below
    // their observation gate, so the preference is the only live channel).
    let conn = open_index(&root);
    let now = recent_now();
    for session in ["a", "b", "c"] {
        record_and_learn(&root, &conn, "crop_yield", Pick::SrcImpl, session, now);
    }
    drop(conn);

    // Sanity: the preference is live in the enabled search.
    let (_, _, learned_why) = search_with_why(&root, false, "crop_yield");
    assert!(
        learned_why.contains("preference"),
        "the preference acts: {learned_why}"
    );

    // THE assertion: --no-feedback reproduces the index-only baseline
    // byte-for-byte — results and why lines (PRD-FB-REQ-017/018, AR-039).
    let (code, free_stdout, free_why) = search_with_why(&root, true, "crop_yield");
    assert_eq!(code, 0);
    assert_eq!(free_stdout, base_stdout, "result lines identical");
    assert_eq!(free_why, base_why, "why stderr identical");
    drop(dir);
}

#[test]
fn preference_replay_is_identical_from_identical_events() {
    // Wipe + replay (watermark reset) vs the original pass: identical
    // preference state from identical events.
    let (dir, root) = learning_repo(true, "");
    let conn = open_index(&root);
    for n in 0..10 {
        record_and_learn(
            &root,
            &conn,
            "crop_yield",
            Pick::SrcImpl,
            &format!("s{n}"),
            5000,
        );
    }
    let first = preference_dump(&conn);

    learning::reset_learned_weights(&conn).unwrap();
    conn.execute("DELETE FROM learned_meta WHERE key = 'event_watermark'", [])
        .unwrap();
    learning::learn_pending(&conn, &feedback_config(true), &fixture_weights(), 5000).unwrap();
    assert_eq!(
        preference_dump(&conn),
        first,
        "wiped and replayed: identical preference state"
    );

    // And two fresh identical repos agree bit-for-bit.
    let build = || {
        let (dir, root) = learning_repo(true, "");
        let conn = open_index(&root);
        for n in 0..10 {
            record_and_learn(
                &root,
                &conn,
                "crop_yield",
                Pick::SrcImpl,
                &format!("s{n}"),
                5000,
            );
        }
        let dump = preference_dump(&conn);
        drop(conn);
        drop(dir);
        dump
    };
    assert_eq!(build(), build(), "bit-identical preferences on replay");
    drop(conn);
    drop(dir);
}

#[test]
fn reset_weights_clears_preferences_and_keeps_events() {
    let (dir, root) = learning_repo(true, "");
    let conn = open_index(&root);
    let now = recent_now();
    for session in ["a", "b", "c"] {
        record_and_learn(&root, &conn, "crop_yield", Pick::SrcImpl, session, now);
    }
    assert_eq!(learning::preference_count(&conn), 1);
    let events: i64 = conn
        .query_row("SELECT COUNT(*) FROM feedback_events", [], |r| r.get(0))
        .unwrap();
    assert_eq!(events, 3);
    let watermark: i64 = conn
        .query_row(
            "SELECT CAST(value AS INTEGER) FROM learned_meta WHERE key = 'event_watermark'",
            [],
            |r| r.get(0),
        )
        .unwrap();

    // --clear-events: history goes, the preference stays (it is learned
    // state, not an event) — TASK-103's independence contract, both ways.
    let (code, _, err) = run_wonk(&root, &["feedback", "--clear-events"]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(
        learning::preference_count(&conn),
        1,
        "clearing events leaves the preference"
    );
    let events_after: i64 = conn
        .query_row("SELECT COUNT(*) FROM feedback_events", [], |r| r.get(0))
        .unwrap();
    assert_eq!(events_after, 0, "the events are gone");

    // --reset-weights: the preference goes with the weights; events are
    // already empty here and the watermark never moves.
    let (code, out, err) = run_wonk(&root, &["feedback", "--reset-weights"]);
    assert_eq!(code, 0, "{err}");
    assert!(
        out.contains("1 result preference(s) cleared"),
        "the confirmation names the preference wipe: {out}"
    );
    assert_eq!(learning::preference_count(&conn), 0);
    let watermark_after: i64 = conn
        .query_row(
            "SELECT CAST(value AS INTEGER) FROM learned_meta WHERE key = 'event_watermark'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(watermark_after, watermark, "the watermark stands");
    drop(conn);
    drop(dir);
}

#[test]
fn feedback_disabled_repo_unchanged_by_preference_tables() {
    // Enabled repo with only a BELOW-GATE preference row: the load is
    // empty and the order is byte-identical to the feature-off repo
    // (PRD-FB-REQ-020).
    let (dir_on, root_on) = learning_repo(true, "");
    let conn = open_index(&root_on);
    conn.execute(
        "INSERT INTO result_preferences \
         (result_identity, strength, observations, sessions, updated_at) \
         VALUES ('inert', 0.2, 5, 2, 1000)",
        [],
    )
    .unwrap();
    assert!(
        load_table(&conn, recent_now()).is_none(),
        "below-gate preference tables change nothing"
    );
    drop(conn);

    // The disabled twin with an ACTIVE-gate row stored: also no influence
    // — the kill switch outranks stored state.
    let (dir_off, root_off) = learning_repo(false, "");
    let conn_off = open_index(&root_off);
    conn_off
        .execute(
            "INSERT INTO result_preferences \
             (result_identity, strength, observations, sessions, updated_at) \
             VALUES ('live-but-off', 0.5, 5, 9, 1000)",
            [],
        )
        .unwrap();
    assert!(
        learning::load_learned(
            &conn_off,
            &feedback_config(false),
            &fixture_weights(),
            recent_now()
        )
        .unwrap()
        .is_none(),
        "feature off means no influence, stored state or not"
    );
    drop(conn_off);

    let (_, out_on, _) = run_wonk(&root_on, &["search", "--include-tests", "crop_yield"]);
    let (_, out_off, _) = run_wonk(&root_off, &["search", "--include-tests", "crop_yield"]);
    assert_eq!(
        result_lines(&out_on),
        result_lines(&out_off),
        "inert tables are byte-identical to feature-off"
    );
    drop(dir_on);
    drop(dir_off);
}

#[test]
fn status_counts_result_preferences() {
    let (dir, root) = learning_repo(true, "");
    let conn = open_index(&root);
    let now = recent_now();
    for session in ["a", "b", "c"] {
        record_and_learn(&root, &conn, "crop_yield", Pick::SrcImpl, session, now);
    }
    drop(conn);
    let (code, _, stderr) = run_wonk(&root, &["status"]);
    assert_eq!(code, 0);
    assert!(
        stderr.contains("3 events, 3 sessions, weight deviation 0.000, 1 result preferences"),
        "the preferences ride the Feedback line: {stderr}"
    );
    let (code, stdout, stderr) = run_wonk(&root, &["status", "--format", "json"]);
    assert_eq!(code, 0, "{stderr}");
    let status: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(status["feedback"]["preferences"], 1);
    drop(dir);
}
