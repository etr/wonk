//! TASK-101 acceptance tests: feedback capture with slate.
//!
//! End-to-end over a tempdir repo indexed with the real `build_index`,
//! driven through the real binary (`wonk search` / `wonk feedback` /
//! `wonk mcp serve`) with a written TOML config enabling `[feedback]`,
//! plus library-level checks for the read APIs. Each test pins one
//! acceptance criterion of the task.

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use rusqlite::Connection;
use serde_json::Value;
use tempfile::TempDir;

use wonk::db;
use wonk::feedback;

// ---------------------------------------------------------------------------
// Fixture
// ---------------------------------------------------------------------------

const AUTH_RS: &str = r#"// Session token helpers for the auth module.
pub fn issue_session_token(user: &User) -> Token {
    let raw = user.secret();
    Token::sign(raw)
}

pub fn validate_session_token(tok: &Token) -> bool {
    tok.verify() && !tok.expired()
}

pub fn refresh_flow(tok: &Token) -> Token {
    issue_session_token(&tok.owner())
}
"#;

const PARSE_PY: &str = r#"# Reads the session_token minted by the Rust side.
def load_session_token(path):
    with open(path) as fh:
        return fh.read().strip()


def cache_session_token(tok):
    CACHE["session_token"] = tok
"#;

/// A repo with the fixture sources and `[feedback]` config, indexed with
/// the real pipeline (local `.wonk/index.db`).
fn feedback_repo(enabled: bool, extra_config: &str) -> (TempDir, PathBuf) {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("fb-repo");
    fs::create_dir_all(root.join("src")).unwrap();
    fs::create_dir(root.join("tools")).unwrap();
    fs::create_dir(root.join(".git")).unwrap();
    fs::write(root.join("src/auth.rs"), AUTH_RS).unwrap();
    fs::write(root.join("tools/parse.py"), PARSE_PY).unwrap();
    fs::create_dir_all(root.join(".wonk")).unwrap();
    fs::write(
        root.join(".wonk/config.toml"),
        format!("[feedback]\nenabled = {enabled}\n{extra_config}"),
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

fn count(conn: &Connection, table: &str) -> i64 {
    conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
        .unwrap()
}

/// The `slate: <token>` trailing line of a text-mode search.
fn slate_line_of(stdout: &str) -> String {
    stdout
        .lines()
        .find(|l| l.starts_with("slate: "))
        .expect("text search prints a slate line")
        .trim_start_matches("slate: ")
        .to_string()
}

/// NDJSON rows of a `--format json` search.
fn json_rows(stdout: &str) -> Vec<Value> {
    stdout
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

// ---------------------------------------------------------------------------
// 9.1 — one MCP call records the slate with alternatives (REQ-001/002/003)
// ---------------------------------------------------------------------------

fn spawn_mcp(root: &Path, home: &Path) -> Child {
    Command::new(wonk_bin())
        .args(["mcp", "serve"])
        .current_dir(root)
        .env("HOME", home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn wonk mcp serve")
}

fn mcp_call(stdin: &mut impl Write, reader: &mut impl BufRead, id: i64, request: &Value) -> Value {
    let mut line = serde_json::to_string(request).unwrap();
    line.push('\n');
    stdin.write_all(line.as_bytes()).unwrap();
    stdin.flush().unwrap();
    let mut response_line = String::new();
    reader.read_line(&mut response_line).unwrap();
    let response: Value = serde_json::from_str(&response_line).unwrap();
    assert_eq!(response["id"], id, "{response}");
    response
}

/// The serve subprocess resolves the CENTRAL index (HOME-relative), so the
/// fixture index is placed there under an isolated HOME.
fn centralize_index(dir: &TempDir, root: &Path) -> PathBuf {
    let home = dir.path().join("home");
    // The serve subprocess resolves the repo root through
    // find_repo_root, which canonicalizes (symlinks: /var ->
    // /private/var on macOS) — the hash must be computed over the same
    // canonical path or the two sides disagree.
    let canon = fs::canonicalize(root).unwrap();
    let hash_dir = home.join(".wonk").join("repos").join(db::repo_hash(&canon));
    fs::create_dir_all(&hash_dir).unwrap();
    fs::copy(root.join(".wonk/index.db"), hash_dir.join("index.db")).unwrap();
    db::write_meta(
        &hash_dir.join("index.db"),
        root,
        &["rust".to_string(), "python".to_string()],
        &[],
    )
    .unwrap();
    hash_dir
}

#[test]
fn mcp_one_call_records_slate_with_alternatives() {
    let (dir, root) = feedback_repo(true, "");
    let hash_dir = centralize_index(&dir, &root);

    let mut child = spawn_mcp(&root, &dir.path().join("home"));
    let mut stdin = child.stdin.take().unwrap();
    let mut reader = BufReader::new(child.stdout.take().unwrap());

    mcp_call(
        &mut stdin,
        &mut reader,
        1,
        &serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {"protocolVersion": "2025-11-25", "capabilities": {},
                       "clientInfo": {"name": "t", "version": "0"}}
        }),
    );
    stdin
        .write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n")
        .unwrap();
    stdin.flush().unwrap();

    let search = mcp_call(
        &mut stdin,
        &mut reader,
        2,
        &serde_json::json!({
            "jsonrpc": "2.0", "id": 2, "method": "tools/call",
            "params": {"name": "wonk_search",
                       "arguments": {"query": "session_token", "format": "json"}}
        }),
    );
    assert!(
        !search["result"]["isError"].as_bool().unwrap_or(false),
        "{search}"
    );
    let rows: Vec<Value> =
        serde_json::from_str(search["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert!(rows.len() >= 3, "the fixture query is multi-result");
    let slate = rows[0]["slate"]
        .as_str()
        .expect("rows carry the slate")
        .to_string();
    let identity = rows[0]["identity"]
        .as_str()
        .expect("rows carry identities")
        .to_string();

    let fb = mcp_call(
        &mut stdin,
        &mut reader,
        3,
        &serde_json::json!({
            "jsonrpc": "2.0", "id": 3, "method": "tools/call",
            "params": {"name": "wonk_feedback",
                       "arguments": {"slate": slate, "useful": [identity], "session": "conv-9"}}
        }),
    );
    assert!(!fb["result"]["isError"].as_bool().unwrap_or(false), "{fb}");
    let summary: Value =
        serde_json::from_str(fb["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(summary["recorded"], 1, "{summary}");

    // The event's features JSON contains EVERY slate member with its rank
    // and `chosen` set only on the useful one.
    let conn = db::open(&hash_dir.join("index.db")).unwrap();
    assert_eq!(count(&conn, "feedback_events"), 1);
    assert_eq!(count(&conn, "feedback_slates"), 1);
    let features: String = conn
        .query_row("SELECT features FROM feedback_events", [], |r| r.get(0))
        .unwrap();
    let v: Value = serde_json::from_str(&features).unwrap();
    let members = v["members"].as_array().unwrap();
    assert!(members.len() >= 3, "alternatives included: {members:?}");
    assert!(
        members
            .iter()
            .all(|m| m["rank"].as_u64().is_some() && m["identity"].as_str().is_some())
    );
    let chosen: Vec<&Value> = members.iter().filter(|m| m["chosen"] == true).collect();
    assert_eq!(chosen.len(), 1);
    assert_eq!(chosen[0]["identity"], identity.as_str());

    drop(stdin);
    let _ = child.wait();
}

// ---------------------------------------------------------------------------
// 9.2 — CLI surface: slate line + ranks or identities (REQ-003)
// ---------------------------------------------------------------------------

#[test]
fn cli_feedback_ranks_or_identities() {
    let (dir, root) = feedback_repo(true, "");
    let (code, stdout, stderr) = run_wonk(&root, &["search", "session_token"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    let token = slate_line_of(&stdout);
    assert_eq!(token.len(), 16);

    // Ranks, comma-split.
    let (code, stdout, stderr) = run_wonk(
        &root,
        &[
            "feedback",
            "--slate",
            &token,
            "--session",
            "cli-1",
            "--useful",
            "2,3",
        ],
    );
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(stdout.contains("recorded 2 event(s)"), "{stdout}");
    let conn = open_index(&root);
    assert_eq!(count(&conn, "feedback_events"), 2);
    drop(conn);
    drop(dir);
}

// ---------------------------------------------------------------------------
// 9.3 — query class recorded at query time (REQ-008)
// ---------------------------------------------------------------------------

#[test]
fn query_class_recorded_at_query_time() {
    let (dir, root) = feedback_repo(true, "");

    run_wonk(&root, &["search", "session_token"]); // symbol-shaped
    // Natural language, matching the fixture's module comment. --smart
    // forces the ranked branch (a symbol-less query alone would not).
    run_wonk(
        &root,
        &["search", "--smart", "token helpers for the auth module"],
    );

    let conn = open_index(&root);
    let mut stmt = conn
        .prepare("SELECT query, query_class FROM feedback_slates ORDER BY created_at, token")
        .unwrap();
    let rows: Vec<(String, Option<String>)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .flatten()
        .collect();
    assert!(rows.len() >= 2, "{rows:?}");
    assert!(
        rows.iter()
            .any(|(q, c)| q == "session_token" && c.as_deref() == Some("symbol")),
        "{rows:?}"
    );
    assert!(
        rows.iter()
            .any(|(q, c)| q.contains("helpers") && c.as_deref() == Some("conceptual")),
        "{rows:?}"
    );
    drop(dir);
}

// ---------------------------------------------------------------------------
// 9.4 — stored vectors are the same values --why displays (THE AC)
// ---------------------------------------------------------------------------

#[test]
fn stored_vectors_match_why_display() {
    let (dir, root) = feedback_repo(true, "");

    // The --why --format json run: each row carries why.signals AND the
    // slate reference recorded from the SAME ranked search.
    let (code, stdout, stderr) = run_wonk(
        &root,
        &["search", "--why", "--format", "json", "session_token"],
    );
    assert_eq!(code, 0, "stderr: {stderr}");
    let rows = json_rows(&stdout);
    assert!(rows.len() >= 3);
    let token = rows[0]["slate"]
        .as_str()
        .expect("slate recorded")
        .to_string();

    // Report one useful result through the persisted slate.
    let (code, _, stderr) = run_wonk(
        &root,
        &[
            "feedback",
            "--slate",
            &token,
            "--session",
            "why-1",
            "--useful",
            "1",
        ],
    );
    assert_eq!(code, 0, "stderr: {stderr}");

    // Parse the stored features and match members to displayed rows by
    // (file, line): the comparison crosses the rendered-artifact /
    // stored-artifact boundary — nothing is shared by construction.
    let conn = open_index(&root);
    let features: String = conn
        .query_row("SELECT features FROM feedback_events", [], |r| r.get(0))
        .unwrap();
    let stored: Value = serde_json::from_str(&features).unwrap();
    let members = stored["members"].as_array().unwrap();

    for row in &rows {
        let file = row["file"].as_str().unwrap();
        let line = row["line"].as_u64().unwrap();
        let member = members
            .iter()
            .find(|m| m["file"].as_str() == Some(file) && m["line"].as_u64() == Some(line))
            .unwrap_or_else(|| panic!("no member for {file}:{line}: {members:?}"));
        let shown = row["why"]["signals"].as_array().unwrap();
        let recorded = member["groups"]["signals"].as_array().unwrap();
        assert_eq!(
            shown.len(),
            recorded.len(),
            "signal count differs for {file}:{line}"
        );
        for (s, r) in shown.iter().zip(recorded.iter()) {
            assert_eq!(s["signal"], r["signal"], "{file}:{line}");
            assert_eq!(s["value"], r["value"], "{file}:{line}");
            assert_eq!(s["weight"], r["weight"], "{file}:{line}");
            assert_eq!(s["weighted"], r["weighted"], "{file}:{line}");
        }
    }
    drop(dir);
}

// ---------------------------------------------------------------------------
// 9.5 — re-index survival and retirement (REQ-005/006)
// ---------------------------------------------------------------------------

#[test]
fn reindex_survival_and_retirement() {
    let (dir, root) = feedback_repo(true, "");
    let (code, stdout, stderr) = run_wonk(&root, &["search", "session_token"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    let token = slate_line_of(&stdout);
    let (code, _, stderr) = run_wonk(
        &root,
        &[
            "feedback",
            "--slate",
            &token,
            "--session",
            "s",
            "--useful",
            "1",
        ],
    );
    assert_eq!(code, 0, "stderr: {stderr}");

    let conn = open_index(&root);
    let identity: String = conn
        .query_row("SELECT result_identity FROM feedback_events", [], |r| {
            r.get(0)
        })
        .unwrap();
    let files: Vec<String> = vec!["src/auth.rs".to_string(), "tools/parse.py".to_string()];
    let queried = std::collections::HashSet::from([identity.clone()]);

    // Unrelated-file edit + body-only edit: the identity survives.
    fs::write(
        root.join("tools/parse.py"),
        PARSE_PY.replace("CACHE[", "GLOBALS["),
    )
    .unwrap();
    wonk::pipeline::build_index(&root, true).unwrap();
    let body_only = AUTH_RS.replace("Token::sign(raw)", "Token::sign(raw.clone())");
    fs::write(root.join("src/auth.rs"), body_only).unwrap();
    wonk::pipeline::build_index(&root, true).unwrap();
    let conn = open_index(&root);
    assert!(
        feedback::live_identities(&conn, &files, &queried).contains(&identity),
        "unrelated + body-only edits must not retire the entry"
    );

    // Signature edit (rename a parameter): the identity retires.
    let reparam = AUTH_RS.replace("user: &User", "owner: &User");
    fs::write(root.join("src/auth.rs"), reparam).unwrap();
    wonk::pipeline::build_index(&root, true).unwrap();
    let conn = open_index(&root);
    assert!(
        !feedback::live_identities(&conn, &files, &queried).contains(&identity),
        "editing the referenced code must retire the entry"
    );
    drop(conn);
    drop(dir);
}

// ---------------------------------------------------------------------------
// 9.6 — no index data written on the query path (action item 10)
// ---------------------------------------------------------------------------

const INDEX_TABLES: &[&str] = &[
    "symbols",
    "\"references\"",
    "files",
    "term_stats",
    "embeddings",
    "reach",
    "reach_truncated",
    "contracts",
    "review_suppressions",
    "file_churn",
    "mined_commits",
    "commit_files",
    "co_change",
    "symbol_topology",
    "symbol_shingles",
    "near_duplicates",
    "summaries",
];

#[test]
fn no_index_writes_on_query_path() {
    let (dir, root) = feedback_repo(true, "");

    let (code, stdout, stderr) = run_wonk(&root, &["search", "session_token"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    let token = slate_line_of(&stdout);

    let before: Vec<(String, i64)> = {
        let conn = open_index(&root);
        INDEX_TABLES
            .iter()
            .map(|t| (t.to_string(), count(&conn, t)))
            .collect()
    };

    let (code, _, stderr) = run_wonk(
        &root,
        &[
            "feedback",
            "--slate",
            &token,
            "--session",
            "s",
            "--useful",
            "1",
        ],
    );
    assert_eq!(code, 0, "stderr: {stderr}");

    let conn = open_index(&root);
    for (table, rows) in &before {
        assert_eq!(
            count(&conn, table),
            *rows,
            "query path wrote to index table {table}"
        );
    }
    // The only growth is in the feedback tables.
    assert_eq!(count(&conn, "feedback_slates"), 1);
    assert_eq!(count(&conn, "feedback_events"), 1);
    drop(conn);
    drop(dir);
}

// ---------------------------------------------------------------------------
// 9.7 — one session distinguishable from twenty (REQ-015)
// ---------------------------------------------------------------------------

#[test]
fn one_session_distinguishable_from_twenty() {
    let (dir, root) = feedback_repo(true, "");
    let conn = open_index(&root);
    let token = feedback::build_and_store_slate(
        &conn,
        "session_token",
        &ranked_for(&root, &conn, "session_token"),
        &wonk::config::FeedbackConfig::default(),
    )
    .unwrap()
    .token;
    let identity: String = {
        let members_json: String = conn
            .query_row(
                "SELECT members FROM feedback_slates WHERE token = ?1",
                [&token],
                |r| r.get(0),
            )
            .unwrap();
        serde_json::from_str::<Vec<feedback::SlateMember>>(&members_json).unwrap()[0]
            .identity
            .clone()
    };

    let record = |session: &str| {
        feedback::record_feedback(&conn, &token, &["1".to_string()], session).unwrap();
    };
    for n in 0..5 {
        record("one-session");
        record(&format!("five-{n}"));
    }
    for n in 0..10 {
        record(&format!("twenty-{n}"));
    }
    assert_eq!(feedback::distinct_sessions(&conn, &identity), 16);
    let other = feedback::result_identity("x.rs", "function", "never", "fn never()");
    assert_eq!(feedback::distinct_sessions(&conn, &other), 0);
    drop(conn);
    drop(dir);
}

/// The ranked search the dispatch layer would hold, for lib-level slate
/// building: real text_search into the fixture repo, real pipeline.
fn ranked_for(root: &Path, conn: &Connection, query: &str) -> wonk::rerank::RankedSearch {
    let root_str = root.display().to_string();
    let mut results = wonk::search::text_search(query, false, false, &[root_str]).unwrap();
    for result in &mut results {
        if let Ok(rel) = result.file.strip_prefix(root) {
            result.file = rel.to_path_buf();
        }
    }
    let settings = wonk::rerank::RankSettings {
        use_pipeline: true,
        ..Default::default()
    };
    wonk::rerank::rank_and_explain_classed(&results, Some(conn), query, &settings)
}

// ---------------------------------------------------------------------------
// 9.8 — default config writes nothing (the TASK-100 property)
// ---------------------------------------------------------------------------

#[test]
fn default_config_writes_nothing() {
    let (dir, root) = feedback_repo(false, "");

    let (code, stdout, stderr) = run_wonk(&root, &["search", "--format", "json", "session_token"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    let rows = json_rows(&stdout);
    assert!(!rows.is_empty());
    for row in &rows {
        let obj = row.as_object().unwrap();
        assert!(!obj.contains_key("slate"), "{row}");
        assert!(!obj.contains_key("identity"), "{row}");
    }
    let (code, _, _) = run_wonk(&root, &["search", "session_token"]);
    assert_eq!(code, 0);
    let conn = open_index(&root);
    assert_eq!(count(&conn, "feedback_slates"), 0, "default writes nothing");

    // The feedback call refuses with the enable hint.
    let out = Command::new(wonk_bin())
        .args([
            "feedback",
            "--slate",
            "0000000000000000",
            "--session",
            "s",
            "--useful",
            "1",
        ])
        .current_dir(&root)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("feedback capture is disabled"), "{stderr}");
    assert!(stderr.contains("[feedback] enabled = true"), "{stderr}");
    drop(conn);
    drop(dir);
}

// ---------------------------------------------------------------------------
// 9.9 — retention cap and the expired-token error (boundedness)
// ---------------------------------------------------------------------------

#[test]
fn slate_pruned_to_retention_and_expired_token_errors() {
    let (dir, root) = feedback_repo(true, "slate_retention = 2\n");

    let (_, out1, _) = run_wonk(&root, &["search", "session_token"]);
    let first = slate_line_of(&out1);
    // created_at has second resolution; distinct seconds keep the LRU
    // deterministic (same-second ties break on token).
    std::thread::sleep(std::time::Duration::from_millis(1100));
    let (_, out2, _) = run_wonk(&root, &["search", "validate_session_token"]);
    let second = slate_line_of(&out2);
    std::thread::sleep(std::time::Duration::from_millis(1100));
    let (_, out3, _) = run_wonk(&root, &["search", "refresh_flow"]);
    let third = slate_line_of(&out3);
    assert_ne!(first, third);

    let conn = open_index(&root);
    assert_eq!(count(&conn, "feedback_slates"), 2, "cap enforced");
    let present = |t: &str| {
        conn.query_row(
            "SELECT COUNT(*) FROM feedback_slates WHERE token = ?1",
            [t],
            |r| r.get::<_, i64>(0),
        )
        .unwrap()
    };
    assert_eq!(present(&first), 0, "oldest pruned");
    assert_eq!(present(&second), 1);
    assert_eq!(present(&third), 1);
    drop(conn);

    let (code, _, stderr) = run_wonk(
        &root,
        &[
            "feedback",
            "--slate",
            &first,
            "--session",
            "s",
            "--useful",
            "1",
        ],
    );
    assert_ne!(code, 0);
    assert!(stderr.contains("slate not found"), "{stderr}");
    assert!(stderr.contains("re-run the search"), "{stderr}");
    drop(dir);
}

// ---------------------------------------------------------------------------
// 9.10 — a pre-TASK-101 index migrates (the ensure_* precedent)
// ---------------------------------------------------------------------------

#[test]
fn pre_task101_index_migrates() {
    let (dir, root) = feedback_repo(true, "");

    // Simulate the old index: both feedback tables dropped.
    {
        let conn = open_index(&root);
        conn.execute_batch("DROP TABLE feedback_events; DROP TABLE feedback_slates;")
            .unwrap();
    }

    // The feedback path recreates them and works.
    let (_, out, _) = run_wonk(&root, &["search", "session_token"]);
    let token = slate_line_of(&out);
    let (code, stdout, stderr) = run_wonk(
        &root,
        &[
            "feedback",
            "--slate",
            &token,
            "--session",
            "s",
            "--useful",
            "1",
        ],
    );
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(stdout.contains("recorded 1 event(s)"), "{stdout}");
    drop(dir);
}

// ---------------------------------------------------------------------------
// 9.11 — never transmitted: feedback data stays in the per-repo index (REQ-004)
// ---------------------------------------------------------------------------

#[test]
fn never_transmitted_local_only() {
    let (dir, root) = feedback_repo(true, "");
    // An isolated HOME: nothing may appear under the central repos dir.
    let home = dir.path().join("isolated-home");
    fs::create_dir_all(&home).unwrap();

    let out = Command::new(wonk_bin())
        .arg("--quiet")
        .arg("search")
        .arg("session_token")
        .current_dir(&root)
        .env("HOME", &home)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    let token = slate_line_of(&String::from_utf8_lossy(&out.stdout));

    let out = Command::new(wonk_bin())
        .args([
            "--quiet",
            "feedback",
            "--slate",
            &token,
            "--session",
            "s",
            "--useful",
            "1",
        ])
        .current_dir(&root)
        .env("HOME", &home)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));

    let repos = home.join(".wonk").join("repos");
    if repos.exists() {
        let entries: Vec<_> = fs::read_dir(&repos).unwrap().flatten().collect();
        assert!(
            entries.is_empty(),
            "feedback data leaked to the global state: {entries:?}"
        );
    }
    // And the recorded data lives in the per-repo index.
    let conn = open_index(&root);
    assert_eq!(count(&conn, "feedback_events"), 1);
    drop(conn);
    drop(dir);
}
