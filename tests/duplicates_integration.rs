//! TASK-100 acceptance tests: shingle signatures and the novelty signal.
//!
//! Library-level over a tempdir repo indexed with the real
//! `build_index`, ranked through the real `text_search` →
//! `rank_and_explain_classed` path with a written TOML config enabling
//! `[rank.weights] novelty`, plus one binary-level `wonk duplicates` run.
//! Each test pins one acceptance criterion of the task end to end.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

use rusqlite::Connection;
use tempfile::TempDir;
use wonk::db;
use wonk::pipeline;
use wonk::rerank::{self, RankSettings};
use wonk::search::{self, SearchResult};

// ---------------------------------------------------------------------------
// Fixture
// ---------------------------------------------------------------------------

/// Five byte-identical copy-pasted handlers, one per file.
const COPY_HANDLER: &str = "pub fn handle_user_created(event: &CreateEvent, store: &mut Store) -> Result<(), Error> {\n    let user = event.payload_user();\n    if user.email.is_empty() {\n        return Err(Error::Validation(\"email required\"));\n    }\n    let existing = store.find_by_email(&user.email)?;\n    if existing.is_some() {\n        return Err(Error::Conflict(\"email already registered\"));\n    }\n    let record = store.insert(&user)?;\n    metrics::count(\"user_signup\", 1);\n    notifier::welcome(&record.email)?;\n    audit::log(\"user_signup\", record.id);\n    Ok(())\n}\n";

const DISTINCT_A: &str = "pub fn process_created_orders(orders: &mut [Order]) -> usize {\n    let mut shipped = 0;\n    for order in orders.iter_mut() {\n        if order.is_paid() {\n            order.ship();\n            shipped += 1;\n        }\n    }\n    shipped\n}\n";

const DISTINCT_B: &str = "pub fn created_at_report(rows: &[Row]) -> String {\n    let mut out = String::new();\n    for row in rows {\n        out.push_str(&row.timestamp.to_string());\n        out.push('\\n');\n    }\n    out\n}\n";

/// Boilerplate-adjacent getters: same shape, one varying tail line.
const GETTER_HEAD: &str = "pub fn get_setting_value(key: &str) -> Option<String> {\n    let source = SETTINGS.lock().unwrap();\n    let raw = source.fetch(key)?;\n    if raw.is_empty() {\n        return None;\n    }\n    ";
const GETTER_TAILS: [&str; 5] = [
    "Some(raw.trim().to_uppercase())\n}\n",
    "Some(expand_env_and_trim(raw))\n}\n",
    "raw.trim().parse::<u64>().map(|v| v.to_string())\n}\n",
    "Some(decode(raw.trim(), Charset::Ascii))\n}\n",
    "Some(raw.split('\\n').next().map(str::to_owned))\n}\n",
];

/// A repo with five copy-pasted handlers (`a*.rs`), two distinct
/// functions matching the same query (`m*.rs`), and a config enabling
/// the novelty signal, indexed with the real pipeline.
fn duplicates_repo() -> (TempDir, Connection) {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    fs::create_dir(root.join(".git")).unwrap();
    for i in 1..=5 {
        fs::write(root.join(format!("a{i}.rs")), COPY_HANDLER).unwrap();
    }
    fs::write(root.join("m1.rs"), DISTINCT_A).unwrap();
    fs::write(root.join("m2.rs"), DISTINCT_B).unwrap();
    write_rank_config(root, 0.8);
    pipeline::build_index(root, true).unwrap();
    let index_path = db::find_existing_index(root).expect("fixture index to exist");
    let conn = db::open(&index_path).unwrap();
    (dir, conn)
}

/// A repo of five similar-but-distinct getters (pairwise sketch Jaccard
/// strictly between 0.5 and the 0.85 threshold).
fn getters_repo() -> (TempDir, Connection) {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    fs::create_dir(root.join(".git")).unwrap();
    for (i, tail) in GETTER_TAILS.iter().enumerate() {
        fs::write(
            root.join(format!("g{}.rs", i + 1)),
            format!("{GETTER_HEAD}{tail}"),
        )
        .unwrap();
    }
    write_rank_config(root, 0.8);
    pipeline::build_index(root, true).unwrap();
    let index_path = db::find_existing_index(root).expect("fixture index to exist");
    let conn = db::open(&index_path).unwrap();
    (dir, conn)
}

fn write_rank_config(root: &Path, novelty: f32) {
    fs::create_dir_all(root.join(".wonk")).unwrap();
    fs::write(
        root.join(".wonk/config.toml"),
        format!("[rank]\nenabled = true\n\n[rank.weights]\nkind = 1.0\nnovelty = {novelty}\n"),
    )
    .unwrap();
}

fn settings_for(root: &Path) -> RankSettings {
    let config = wonk::config::Config::load(Some(root)).expect("fixture config loads");
    RankSettings::from_config(
        &config.rank,
        &config.search,
        config.embedding.provider,
        None,
        config.topology.enabled,
        config.duplicate.threshold,
    )
    .expect("fixture config is a valid rank config")
}

fn candidates(root: &Path, query: &str) -> Vec<SearchResult> {
    let root_str = root.display().to_string();
    let mut results = search::text_search(query, false, false, &[root_str]).unwrap();
    for result in &mut results {
        if let Ok(rel) = result.file.strip_prefix(root) {
            result.file = rel.to_path_buf();
        }
    }
    results
}

/// Ranked file names in emission order (groups flattened).
fn ranked_files(root: &Path, conn: &Connection, query: &str) -> Vec<String> {
    let ranked = rerank::rank_and_explain_classed(
        &candidates(root, query),
        Some(conn),
        query,
        &settings_for(root),
    );
    ranked
        .groups
        .iter()
        .flat_map(|(_, items)| items.iter())
        .map(|s| s.classified.result.file.to_string_lossy().into_owned())
        .collect()
}

// ---------------------------------------------------------------------------
// AC 1: five copy-pasted handlers
// ---------------------------------------------------------------------------

#[test]
fn ac_five_copy_pasted_handlers() {
    let (dir, conn) = duplicates_repo();
    let root = dir.path();

    // Novelty OFF baseline: five equal hits, copies ahead of the
    // distinct content by the file tie-break.
    write_rank_config(root, 0.0);
    let off = ranked_files(root, &conn, "created");
    assert_eq!(
        off,
        vec![
            "a1.rs", "a2.rs", "a3.rs", "a4.rs", "a5.rs", "m1.rs", "m2.rs"
        ],
        "novelty off: five equal hits with copies first"
    );

    // Novelty ON (the fixture config): one representative ranked among
    // the distinct results, the four copies demoted below them.
    write_rank_config(root, 0.8);
    let on = ranked_files(root, &conn, "created");
    assert_eq!(
        on,
        vec![
            "a1.rs", "m1.rs", "m2.rs", "a2.rs", "a3.rs", "a4.rs", "a5.rs"
        ]
    );

    // The why path renders a novelty contribution for every result.
    let ranked = rerank::rank_and_explain_classed(
        &candidates(root, "created"),
        Some(&conn),
        "created",
        &settings_for(root),
    );
    let items: Vec<&rerank::ScoredResult> =
        ranked.groups.iter().flat_map(|(_, g)| g.iter()).collect();
    assert_eq!(items.len(), 7);
    for item in &items {
        let why = wonk::output::WhyOutput::from_contributions(item.score, &item.contributions);
        let line = wonk::output::format_why_line(
            &item.classified.result.file.to_string_lossy(),
            item.classified.result.line,
            why.total,
            &why.signals,
        );
        assert!(line.contains("novelty"), "why line lacks novelty: {line}");
    }
    // The representative is undemoted; every copy is fully redundant.
    let novelty_of = |file: &str| -> f32 {
        items
            .iter()
            .find(|s| s.classified.result.file.to_string_lossy() == file)
            .and_then(|s| s.contributions.iter().find(|c| c.signal == "novelty"))
            .map(|c| c.value)
            .unwrap()
    };
    assert_eq!(novelty_of("a1.rs"), 1.0);
    assert_eq!(novelty_of("m1.rs"), 1.0);
    for copy in ["a2.rs", "a3.rs", "a4.rs", "a5.rs"] {
        assert_eq!(novelty_of(copy), 0.0);
    }
}

// ---------------------------------------------------------------------------
// AC 2: no body reads at query time
// ---------------------------------------------------------------------------

static TRACE_SQL: Mutex<Vec<String>> = Mutex::new(Vec::new());

fn trace_stmts(event: rusqlite::trace::TraceEvent<'_>) {
    if let rusqlite::trace::TraceEvent::Stmt(_, sql) = event
        && let Ok(mut log) = TRACE_SQL.lock()
    {
        log.push(sql.to_string());
    }
}

#[test]
fn ac_no_body_reads_at_query_time() {
    let (dir, conn) = duplicates_repo();
    let root = dir.path();
    let settings = settings_for(root);

    TRACE_SQL.lock().unwrap().clear();
    conn.trace_v2(
        rusqlite::trace::TraceEventCodes::SQLITE_TRACE_STMT,
        Some(trace_stmts),
    );
    let ranked = rerank::rank_and_explain_classed(
        &candidates(root, "created"),
        Some(&conn),
        "created",
        &settings,
    );
    conn.trace_v2(rusqlite::trace::TraceEventCodes::SQLITE_TRACE_STMT, None);
    assert_eq!(ranked.groups.len(), 1, "search succeeded under the trace");

    let statements = TRACE_SQL.lock().unwrap().clone();
    assert!(!statements.is_empty());

    // Similarity really used the signature blobs.
    assert!(
        statements.iter().any(|s| s.contains("symbol_shingles")),
        "no statement read the sketches: {statements:?}"
    );
    // Every sketch statement touches ONLY symbol_shingles (the presence
    // probe) or symbols+symbol_shingles (the loader join) — never the
    // file table, references, or any other source: no body can be
    // pulled this way.
    for stmt in &statements {
        if stmt.contains("symbol_shingles") {
            let lowered = stmt.to_lowercase();
            assert!(
                !lowered.contains("files")
                    && !lowered.contains("\"references\"")
                    && !lowered.contains("embeddings")
                    && !lowered.contains("term_stats"),
                "sketch statement reads beyond symbols: {stmt}"
            );
        }
    }
    // And the ranked path never touches the files table at all.
    assert!(
        statements
            .iter()
            .all(|s| !s.to_lowercase().contains("from files")),
        "ranking read the files table: {statements:?}"
    );
}

// ---------------------------------------------------------------------------
// AC 3: a duplicate group always yields at least one result
// ---------------------------------------------------------------------------

#[test]
fn ac_duplicate_group_yields_representative() {
    let (dir, conn) = duplicates_repo();
    let root = dir.path();
    let ranked = rerank::rank_and_explain_classed(
        &candidates(root, "created"),
        Some(&conn),
        "created",
        &settings_for(root),
    );
    let flat: Vec<String> = ranked
        .groups
        .iter()
        .flat_map(|(_, items)| items.iter())
        .map(|s| s.classified.result.file.to_string_lossy().into_owned())
        .collect();
    assert_eq!(flat.len(), 7);

    // Any budget/limit cut that keeps a copy must keep the
    // representative — the representative outranks every copy, so no
    // truncation can decapitate the group.
    for limit in 1..=7 {
        let top: Vec<&String> = flat.iter().take(limit).collect();
        let copies = top
            .iter()
            .filter(|f| f.as_str() != "a1.rs" && f.starts_with('a'))
            .count();
        if copies > 0 {
            assert!(
                top.iter().any(|f| f.as_str() == "a1.rs"),
                "limit {limit} kept {copies} copies without the representative"
            );
        }
    }
    assert!(flat.iter().take(1).any(|f| f == "a1.rs"));
}

// ---------------------------------------------------------------------------
// REQ-003: pairs recorded after search
// ---------------------------------------------------------------------------

#[test]
fn req003_pairs_recorded_after_search() {
    let (dir, conn) = duplicates_repo();
    let root = dir.path();
    let ranked = rerank::rank_and_explain_classed(
        &candidates(root, "created"),
        Some(&conn),
        "created",
        &settings_for(root),
    );

    // The novelty pass surfaced the C(5, 2) pairs of the identical group.
    assert_eq!(ranked.near_duplicates.len(), 10);
    assert!(
        ranked
            .near_duplicates
            .iter()
            .all(|p| (p.similarity - 1.0).abs() < 1e-6)
    );

    // The dispatch layer records them, canonically.
    wonk::shingles::record_pairs_best_effort(Some(&conn), &ranked.near_duplicates);
    let rows: Vec<(i64, i64, f32)> = {
        let mut stmt = conn
            .prepare("SELECT symbol_id_a, symbol_id_b, similarity FROM near_duplicates")
            .unwrap();
        stmt.query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, f32>(2)?,
            ))
        })
        .unwrap()
        .flatten()
        .collect()
    };
    assert_eq!(rows.len(), 10);
    assert!(rows.iter().all(|(a, b, _)| a < b), "canonical a < b");
    assert!(rows.iter().all(|(_, _, sim)| (*sim - 1.0).abs() < 1e-6));
}

// ---------------------------------------------------------------------------
// REQ-006: `wonk duplicates` reporting (binary level)
// ---------------------------------------------------------------------------

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

fn run_duplicates(repo: &Path, extra: &[&str]) -> (i32, String) {
    let mut cmd = Command::new(wonk_bin());
    cmd.arg("--quiet")
        .arg("duplicates")
        .args(extra)
        .current_dir(repo);
    let out = cmd.output().unwrap();
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
    )
}

#[test]
fn req006_duplicates_command_reports_groups() {
    let (dir, _conn) = duplicates_repo();
    let root = dir.path();

    let (code, stdout) = run_duplicates(root, &[]);
    assert_eq!(code, 0, "stderr issues aside, the command succeeds");
    assert!(
        stdout.contains("dup-group 1 size=5 mean-sim=1.00"),
        "group of five with mean similarity: {stdout}"
    );
    for i in 1..=5 {
        assert!(
            stdout.contains(&format!("a{i}.rs:1 function handle_user_created")),
            "member line missing: {stdout}"
        );
    }
    // Singletons never appear.
    assert!(!stdout.contains("m1.rs"), "{stdout}");

    // The threshold knob filters: the getters repo's pairs sit near 0.6,
    // so the default 0.85 reports nothing while 0.5 finds them.
    let (getters_dir, _gconn) = getters_repo();
    let (code, stdout) = run_duplicates(getters_dir.path(), &[]);
    assert_eq!(code, 0);
    assert!(
        !stdout.contains("dup-group"),
        "default 0.85 empty: {stdout}"
    );
    let (code, stdout) = run_duplicates(getters_dir.path(), &["--threshold", "0.5"]);
    assert_eq!(code, 0);
    assert!(
        stdout.contains("dup-group"),
        "0.5 finds the pairs: {stdout}"
    );
}

// ---------------------------------------------------------------------------
// AR-041: boilerplate is not over-demoted
// ---------------------------------------------------------------------------

#[test]
fn boilerplate_not_overdemoted() {
    // Fixture sanity: every getter pair sits strictly between 0.5 and
    // the 0.85 threshold — similar, but not near-duplicates.
    let bodies: Vec<String> = GETTER_TAILS
        .iter()
        .map(|tail| format!("{GETTER_HEAD}{tail}"))
        .collect();
    let sketches: Vec<Vec<u32>> = bodies
        .iter()
        .map(|b| wonk::shingles::body_signature(b))
        .collect();
    for i in 0..sketches.len() {
        for j in i + 1..sketches.len() {
            let sim = wonk::shingles::sketch_jaccard(&sketches[i], &sketches[j]);
            assert!(
                (0.5..0.85).contains(&sim),
                "getter pair {i}/{j} out of range: {sim}"
            );
        }
    }

    let (dir, conn) = getters_repo();
    let root = dir.path();
    let query = "setting";
    assert_eq!(candidates(root, query).len(), 5);

    // Novelty on: every getter stays novel (1.0) and the order is
    // byte-identical to the novelty-off run.
    let on = ranked_files(root, &conn, query);
    write_rank_config(root, 0.0);
    let off = ranked_files(root, &conn, query);
    assert_eq!(on, off, "sub-threshold similarity must not reorder");

    write_rank_config(root, 0.8);
    let ranked = rerank::rank_and_explain_classed(
        &candidates(root, query),
        Some(&conn),
        query,
        &settings_for(root),
    );
    for (_, items) in &ranked.groups {
        for item in items {
            let value = item
                .contributions
                .iter()
                .find(|c| c.signal == "novelty")
                .map(|c| c.value)
                .expect("novelty row present");
            assert_eq!(value, 1.0, "no boilerplate demotion");
        }
    }
}
