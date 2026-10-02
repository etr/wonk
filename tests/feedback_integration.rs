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

const MINT_RS: &str = r#"// Token minting for the nested auth module.
pub fn mint_session_token(user: &User) -> Token {
    issue_session_token(user)
}
"#;

/// A repo with the fixture sources and `[feedback]` config, indexed with
/// the real pipeline (local `.wonk/index.db`).
fn feedback_repo(enabled: bool, extra_config: &str) -> (TempDir, PathBuf) {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("fb-repo");
    fs::create_dir_all(root.join("src/auth/tokens")).unwrap();
    fs::create_dir_all(root.join("tools")).unwrap();
    fs::create_dir_all(root.join(".git")).unwrap();
    fs::write(root.join("src/auth.rs"), AUTH_RS).unwrap();
    fs::write(root.join("src/auth/tokens/mint.rs"), MINT_RS).unwrap();
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
    // Report the src/auth.rs member: the retirement edits below target
    // that file, and rank 1 may live elsewhere in the multi-file fixture.
    let auth_rank: String = {
        let conn = open_index(&root);
        let (_, _, members_json): (String, Option<String>, String) = conn
            .query_row(
                "SELECT query, query_class, members FROM feedback_slates WHERE token = ?1",
                [&token],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        let members: Vec<Value> = serde_json::from_str(&members_json).unwrap();
        members
            .iter()
            .find(|m| m["file"].as_str().unwrap().ends_with("src/auth.rs"))
            .map(|m| m["rank"].as_u64().unwrap().to_string())
            .expect("a src/auth.rs member in the slate")
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
            &auth_rank,
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

// ---------------------------------------------------------------------------
// TASK-105: result feature extraction acceptance
// ---------------------------------------------------------------------------

/// The members JSON of the newest slate, parsed.
fn newest_members(conn: &Connection) -> Vec<Value> {
    let members: String = conn
        .query_row(
            "SELECT members FROM feedback_slates ORDER BY created_at DESC, token DESC LIMIT 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    serde_json::from_str(&members).unwrap()
}

/// Seed deterministic history rows (the mine's output shape) post-index.
fn seed_history(conn: &Connection) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    conn.execute(
        "INSERT INTO file_churn (file, score, last_ts, last_author, primary_author) VALUES \
         ('src/auth.rs', 5.0, ?1, 'Ada', 'Ada'), \
         ('src/auth/tokens/mint.rs', 12.0, ?2, 'Grace', 'Grace'), \
         ('tools/parse.py', 0.5, ?3, 'Guido', 'Guido')",
        rusqlite::params![now - 3600, now - 40 * 24 * 3600, now - 400 * 24 * 3600],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO co_change (file_a, file_b, weight) \
         VALUES ('src/auth.rs', 'tools/parse.py', 4.0)",
        [],
    )
    .unwrap();
}

/// The ranked search the dispatch layer would hold, with feedback capture
/// (and an optional hint) on — the extraction's real input.
fn ranked_capture(
    root: &Path,
    conn: &Connection,
    query: &str,
    hint: Option<&str>,
) -> wonk::rerank::RankedSearch {
    let root_str = root.display().to_string();
    let mut results = wonk::search::text_search(query, false, false, &[root_str]).unwrap();
    for result in &mut results {
        if let Ok(rel) = result.file.strip_prefix(root) {
            result.file = rel.to_path_buf();
        }
    }
    let settings = wonk::rerank::RankSettings {
        use_pipeline: true,
        feedback_capture: true,
        working_context: hint.map(str::to_string),
        ..Default::default()
    };
    wonk::rerank::rank_and_explain_classed(&results, Some(conn), query, &settings)
}

#[test]
fn nested_result_emits_per_ancestor_features() {
    let (dir, root) = feedback_repo(true, "");
    {
        let conn = open_index(&root);
        seed_history(&conn);
    }
    let (code, stdout, stderr) = run_wonk(&root, &["search", "mint_session_token"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    let token = slate_line_of(&stdout);
    let conn = open_index(&root);
    let (_, _, members_json): (String, Option<String>, String) = conn
        .query_row(
            "SELECT query, query_class, members FROM feedback_slates WHERE token = ?1",
            [&token],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    let members: Vec<Value> = serde_json::from_str(&members_json).unwrap();
    let mint = members
        .iter()
        .find(|m| {
            m["file"]
                .as_str()
                .unwrap()
                .ends_with("src/auth/tokens/mint.rs")
        })
        .expect("the nested fixture file is in the slate");
    let path = mint["groups"]["path"].as_object().unwrap();
    assert_eq!(path.get("src").and_then(Value::as_str), Some("1"));
    assert_eq!(path.get("src/auth").and_then(Value::as_str), Some("1"));
    assert_eq!(
        path.get("src/auth/tokens").and_then(Value::as_str),
        Some("1")
    );
    assert_eq!(path.get("depth").and_then(Value::as_str), Some("mid"));
    assert_eq!(path.get("class").and_then(Value::as_str), Some("ordinary"));
    drop(conn);
    drop(dir);
}

#[test]
fn recorded_continuous_values_are_labels_not_numbers() {
    let (dir, root) = feedback_repo(true, "");
    {
        let conn = open_index(&root);
        seed_history(&conn);
    }
    let (code, _, stderr) = run_wonk(&root, &["search", "session_token"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    let conn = open_index(&root);
    for member in newest_members(&conn) {
        let groups = &member["groups"];
        let file = member["file"].as_str().unwrap();
        for group in ["history", "graph"] {
            for (name, value) in groups[group].as_object().into_iter().flatten() {
                // `community` is a capped categorical identity
                // (PRD-FB-REQ-024), not a bucketed continuous — every
                // other label in these groups must stay digit-free.
                if group == "graph" && name == "community" {
                    continue;
                }
                let label = value.as_str().unwrap();
                assert!(
                    !label.chars().any(|c| c.is_ascii_digit()),
                    "{file} {group}.{name} leaked a raw value: {label}"
                );
            }
        }
        if let Some(body) = groups["symbol"]["body_size"].as_str() {
            assert!(!body.chars().any(|c| c.is_ascii_digit()), "{file}: {body}");
        }
        // The seeded history is present with the expected buckets.
        if file.ends_with("src/auth/tokens/mint.rs") {
            assert_eq!(groups["history"]["churn"].as_str(), Some("high"));
            assert_eq!(groups["history"]["recency"].as_str(), Some("months"));
            assert_eq!(groups["author"]["primary"].as_str(), Some("Grace"));
        }
    }
    drop(conn);
    drop(dir);
}

#[test]
fn high_cardinality_authors_collapse_into_overflow_in_stored_slate() {
    let (dir, root) = feedback_repo(true, "");
    // 40 more files under distinct directories, one distinct author each.
    for i in 0..40 {
        let file = format!("gen{i:02}/file.rs");
        fs::create_dir_all(root.join(&file).parent().unwrap()).unwrap();
        fs::write(
            root.join(&file),
            format!("pub fn gen_{i}_helper() -> u32 {{ {i} }}\n"),
        )
        .unwrap();
    }
    wonk::pipeline::build_index(&root, true).unwrap();
    {
        let conn = open_index(&root);
        for i in 0..40 {
            conn.execute(
                "INSERT INTO file_churn (file, score, last_ts, last_author, primary_author) \
                 VALUES (?1, 1.0, 100, ?2, ?2)",
                rusqlite::params![format!("gen{i:02}/file.rs"), format!("Author{i:02}")],
            )
            .unwrap();
        }
    }
    let (code, _, stderr) = run_wonk(&root, &["search", "--smart", "gen_.*_helper", "--regex"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    let conn = open_index(&root);
    let members = newest_members(&conn);
    let primaries: Vec<&str> = members
        .iter()
        .filter_map(|m| m["groups"]["author"]["primary"].as_str())
        .collect();
    let kept = primaries.iter().filter(|p| **p != "__overflow__").count();
    let overflowed = primaries.iter().filter(|p| **p == "__overflow__").count();
    assert!(kept <= 32, "{kept} distinct labels survive the cap");
    assert!(
        overflowed >= 8,
        "the beyond-cap values collapse: {overflowed}"
    );
    drop(conn);
    drop(dir);
}

#[test]
fn context_hint_present_vs_absent_cli() {
    let (dir, root) = feedback_repo(true, "");
    {
        let conn = open_index(&root);
        seed_history(&conn);
    }
    // With the hint: members of the hinted file carry same_file=yes and
    // the co-change bucket against the hint's partners.
    let (code, stdout, stderr) = run_wonk(
        &root,
        &["search", "--context", "src/auth.rs", "session_token"],
    );
    assert_eq!(code, 0, "stderr: {stderr}");
    let token = slate_line_of(&stdout);
    let conn = open_index(&root);
    let (_, _, members_json): (String, Option<String>, String) = conn
        .query_row(
            "SELECT query, query_class, members FROM feedback_slates WHERE token = ?1",
            [&token],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    let members: Vec<Value> = serde_json::from_str(&members_json).unwrap();
    let auth_members: Vec<&Value> = members
        .iter()
        .filter(|m| m["file"].as_str().unwrap().ends_with("src/auth.rs"))
        .collect();
    assert!(!auth_members.is_empty());
    for member in &auth_members {
        assert_eq!(
            member["groups"]["context"]["same_file"].as_str(),
            Some("yes"),
            "{}",
            member
        );
        // tools/parse.py co-changes with the hint at weight 4.0: strong.
    }
    let partner = members
        .iter()
        .find(|m| m["file"].as_str().unwrap().ends_with("tools/parse.py"))
        .expect("partner file in slate");
    assert_eq!(
        partner["groups"]["context"]["co_change"].as_str(),
        Some("strong"),
        "{}",
        partner
    );
    assert_eq!(
        partner["groups"]["context"]["same_file"].as_str(),
        Some("no")
    );
    drop(conn);

    // Without the hint: the context key is absent, not defaulted.
    let (code, stdout, stderr) = run_wonk(&root, &["search", "session_token"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    let token = slate_line_of(&stdout);
    let conn = open_index(&root);
    let (_, _, members_json): (String, Option<String>, String) = conn
        .query_row(
            "SELECT query, query_class, members FROM feedback_slates WHERE token = ?1",
            [&token],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert!(
        !members_json.contains("\"context\""),
        "absent rather than defaulted: {members_json}"
    );
    drop(conn);
    drop(dir);
}

#[test]
fn author_features_off_records_no_author_group_cli() {
    let (dir, root) = feedback_repo(true, "author_features = false\n");
    {
        let conn = open_index(&root);
        seed_history(&conn);
    }
    let (code, _, stderr) = run_wonk(&root, &["search", "session_token"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    let conn = open_index(&root);
    let members_json: String = conn
        .query_row(
            "SELECT members FROM feedback_slates ORDER BY created_at DESC, token DESC LIMIT 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        !members_json.contains("\"author\""),
        "switch off removes the group: {}",
        &members_json[..200.min(members_json.len())]
    );
    // Every other descriptive group still records.
    let members: Vec<Value> = serde_json::from_str(&members_json).unwrap();
    assert!(members.iter().all(|m| {
        m["groups"]["path"]
            .as_object()
            .is_some_and(|p| !p.is_empty())
    }));
    drop(conn);
    drop(dir);
}

// ---------------------------------------------------------------------------
// TASK-105 action item 8: no query-time round trips beyond the batched
// prepare (the ac_no_body_reads_at_query_time trace pattern)
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
fn slate_build_reads_only_symbols_and_writes_only_slates() {
    let (dir, root) = feedback_repo(true, "");
    let conn = open_index(&root);
    seed_history(&conn);
    let ranked = ranked_capture(&root, &conn, "session_token", None);
    assert!(!ranked.context.churn_score("src/auth.rs").is_none());

    TRACE_SQL.lock().unwrap().clear();
    conn.trace_v2(
        rusqlite::trace::TraceEventCodes::SQLITE_TRACE_STMT,
        Some(trace_stmts),
    );
    let stored =
        feedback::build_and_store_slate(&conn, "session_token", &ranked, &Default::default())
            .unwrap();
    conn.trace_v2(rusqlite::trace::TraceEventCodes::SQLITE_TRACE_STMT, None);
    assert!(!stored.members.is_empty(), "the slate stored");

    let statements = TRACE_SQL.lock().unwrap().clone();
    assert!(!statements.is_empty());
    for stmt in &statements {
        let lowered = stmt.to_lowercase();
        let touches_symbols = lowered.contains("from symbols");
        let touches_slates = lowered.contains("feedback_slates");
        let is_txn = lowered.starts_with("begin") || lowered.starts_with("commit");
        assert!(
            touches_symbols || touches_slates || is_txn,
            "slate build statement beyond the prepare: {stmt}"
        );
        for forbidden in [
            "file_churn",
            "co_change",
            "symbol_topology",
            "reach",
            "\"references\"",
            "embeddings",
            "term_stats",
        ] {
            assert!(
                !lowered.contains(forbidden),
                "slate build re-read {forbidden}: {stmt}"
            );
        }
    }
    drop(conn);
    drop(dir);
}

/// The ranked search WITHOUT stripping the repo root — the MCP shape:
/// result files stay absolute, so the canonical file keys (D6) are the
/// only thing that maps them onto the repo-relative index.
fn ranked_capture_absolute(
    root: &Path,
    conn: &Connection,
    query: &str,
) -> wonk::rerank::RankedSearch {
    let root_str = root.display().to_string();
    let results = wonk::search::text_search(query, true, false, &[root_str]).unwrap();
    assert!(
        results.iter().any(|r| r.file.is_absolute()),
        "the MCP shape: absolute result paths"
    );
    let settings = wonk::rerank::RankSettings {
        use_pipeline: true,
        feedback_capture: true,
        ..Default::default()
    };
    wonk::rerank::rank_and_explain_classed(&results, Some(conn), query, &settings)
}

/// Statement count of one traced `build_and_store_slate` over the
/// absolute-path shape, with every statement shape-checked against the
/// batched invariant (symbols bulk-load + feedback_slates writes + txn).
fn slate_build_statement_count(root: &Path, conn: &Connection, query: &str) -> usize {
    let ranked = ranked_capture_absolute(root, conn, query);
    let distinct_files: std::collections::BTreeSet<String> = ranked
        .groups
        .iter()
        .flat_map(|(_, g)| g.iter())
        .map(|item| item.classified.result.file.to_string_lossy().into_owned())
        .collect();
    assert!(
        distinct_files.iter().any(|f| Path::new(f).is_absolute()),
        "absolute result paths reached the slate build"
    );

    TRACE_SQL.lock().unwrap().clear();
    conn.trace_v2(
        rusqlite::trace::TraceEventCodes::SQLITE_TRACE_STMT,
        Some(trace_stmts),
    );
    let stored =
        feedback::build_and_store_slate(conn, query, &ranked, &Default::default()).unwrap();
    conn.trace_v2(rusqlite::trace::TraceEventCodes::SQLITE_TRACE_STMT, None);
    assert!(!stored.members.is_empty(), "the slate stored");

    let statements = TRACE_SQL.lock().unwrap().clone();
    assert!(!statements.is_empty());
    for stmt in &statements {
        let lowered = stmt.to_lowercase();
        let touches_symbols = lowered.contains("from symbols");
        let touches_slates = lowered.contains("feedback_slates");
        let is_txn = lowered.starts_with("begin") || lowered.starts_with("commit");
        assert!(
            touches_symbols || touches_slates || is_txn,
            "slate build statement beyond the prepare: {stmt}"
        );
        assert!(
            !lowered.contains("like '%'"),
            "the per-file suffix fallback fired: {stmt}"
        );
    }
    statements.len()
}

#[test]
fn absolute_path_slate_build_is_fixed_cost_not_per_file() {
    // The count-based round-trip gate in the shape where the invariant
    // actually broke: over MCP the result files are absolute, and a
    // slate build that missed the exact `symbols IN` pass would issue
    // ONE suffix-fallback query per distinct file. n=3 vs n=12 distinct
    // files must cost the same number of statements.
    let (dir, root) = feedback_repo(true, "");
    for i in 0..12 {
        let file = format!("abs{i:02}/file.rs");
        fs::create_dir_all(root.join(&file).parent().unwrap()).unwrap();
        fs::write(
            root.join(&file),
            format!("pub fn abs_query_{i:02}_target() -> u32 {{ {i} }}\n"),
        )
        .unwrap();
    }
    wonk::pipeline::build_index(&root, true).unwrap();
    let conn = open_index(&root);

    let three = slate_build_statement_count(&root, &conn, "abs_query_0[0-2]_target");
    let twelve = slate_build_statement_count(&root, &conn, "abs_query_[0-9][0-9]_target");
    assert_eq!(
        three, twelve,
        "slate-build statement count must not scale with distinct files \
         (3 files -> {three}, 12 files -> {twelve})"
    );
    drop(conn);
    drop(dir);
}

#[test]
fn mcp_context_file_feeds_context_features() {
    let (dir, root) = feedback_repo(true, "");
    let hash_dir = centralize_index(&dir, &root);
    // The serve subprocess resolves the central copy: seed the history
    // facts there so co-change/churn context exists over MCP too.
    {
        let conn = db::open(&hash_dir.join("index.db")).unwrap();
        seed_history(&conn);
    }

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

    // With the hint: the hinted file's members carry same_file=yes, and
    // the co-change partner carries its bucket — over ABSOLUTE result
    // paths (the MCP shape the canonical file keys exist for).
    let search = mcp_call(
        &mut stdin,
        &mut reader,
        2,
        &serde_json::json!({
            "jsonrpc": "2.0", "id": 2, "method": "tools/call",
            "params": {"name": "wonk_search",
                       "arguments": {"query": "session_token", "format": "json",
                                     "context_file": "src/auth.rs"}}
        }),
    );
    assert!(
        !search["result"]["isError"].as_bool().unwrap_or(false),
        "{search}"
    );
    let rows: Vec<Value> =
        serde_json::from_str(search["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    let slate = rows[0]["slate"].as_str().unwrap().to_string();

    let conn = db::open(&hash_dir.join("index.db")).unwrap();
    let (_, _, members_json): (String, Option<String>, String) = conn
        .query_row(
            "SELECT query, query_class, members FROM feedback_slates WHERE token = ?1",
            [&slate],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    let members: Vec<Value> = serde_json::from_str(&members_json).unwrap();
    let auth_members: Vec<&Value> = members
        .iter()
        .filter(|m| m["file"].as_str().unwrap().ends_with("src/auth.rs"))
        .collect();
    assert!(!auth_members.is_empty(), "absolute-path members recorded");
    for member in &auth_members {
        assert_eq!(
            member["groups"]["context"]["same_file"].as_str(),
            Some("yes"),
            "{}",
            member
        );
        // The MCP members also carry the descriptive groups the canonical
        // keys make possible (the latent-gap fix working end to end).
        assert!(
            member["groups"]["history"]
                .as_object()
                .is_some_and(|h| !h.is_empty())
        );
    }
    let partner = members
        .iter()
        .find(|m| m["file"].as_str().unwrap().ends_with("tools/parse.py"))
        .expect("partner file in slate");
    assert_eq!(
        partner["groups"]["context"]["co_change"].as_str(),
        Some("strong"),
        "{}",
        partner
    );
    drop(conn);

    // Without the argument: the context key is absent, not defaulted.
    let plain = mcp_call(
        &mut stdin,
        &mut reader,
        3,
        &serde_json::json!({
            "jsonrpc": "2.0", "id": 3, "method": "tools/call",
            "params": {"name": "wonk_search",
                       "arguments": {"query": "session_token", "format": "json"}}
        }),
    );
    assert!(
        !plain["result"]["isError"].as_bool().unwrap_or(false),
        "{plain}"
    );
    let rows: Vec<Value> =
        serde_json::from_str(plain["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    let slate = rows[0]["slate"].as_str().unwrap().to_string();
    let conn = db::open(&hash_dir.join("index.db")).unwrap();
    let members_json: String = conn
        .query_row(
            "SELECT members FROM feedback_slates WHERE token = ?1",
            [&slate],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        !members_json.contains("\"context\""),
        "absent rather than defaulted: {}",
        &members_json[..300.min(members_json.len())]
    );

    drop(stdin);
    let _ = child.wait();
}
