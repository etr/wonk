//! Integration tests for the MCP server (`wonk mcp serve`).
//!
//! Spawns the server as a subprocess with piped stdin/stdout and verifies
//! the JSON-RPC handshake and tool listing. The degraded embedding-provider
//! tests additionally pin the child process's proxy environment (as in
//! tests/ask_integration.rs) so the configured Ollama is deterministically
//! unreachable.

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use serde_json::Value;

/// Build the binary path. In test mode, cargo puts it in target/debug/.
fn wonk_bin() -> std::path::PathBuf {
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

fn send_and_recv(stdin: &mut impl Write, reader: &mut impl BufRead, request: &Value) -> Value {
    let mut line = serde_json::to_string(request).unwrap();
    line.push('\n');
    stdin.write_all(line.as_bytes()).unwrap();
    stdin.flush().unwrap();

    let mut response_line = String::new();
    reader.read_line(&mut response_line).unwrap();
    serde_json::from_str(&response_line).unwrap()
}

fn send_notification(stdin: &mut impl Write, notification: &Value) {
    let mut line = serde_json::to_string(notification).unwrap();
    line.push('\n');
    stdin.write_all(line.as_bytes()).unwrap();
    stdin.flush().unwrap();
}

#[test]
fn mcp_server_initialize_and_list_tools() {
    let bin = wonk_bin();
    if !bin.exists() {
        panic!("wonk binary not found at {}", bin.display());
    }

    // Use a temp dir as the repo root to avoid interfering with the real repo.
    let tmp = tempfile::tempdir().unwrap();
    // Initialize a git repo so find_repo_root succeeds.
    Command::new("git")
        .args(["init"])
        .current_dir(tmp.path())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();

    let mut child = Command::new(&bin)
        .args(["mcp", "serve"])
        .current_dir(tmp.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn wonk mcp serve");

    let mut stdin = child.stdin.take().unwrap();
    let mut reader = BufReader::new(child.stdout.take().unwrap());

    // 1. Send initialize request.
    let init_req = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-11-25",
            "capabilities": {},
            "clientInfo": {"name": "test", "version": "0.1"}
        }
    });
    let init_resp = send_and_recv(&mut stdin, &mut reader, &init_req);

    assert_eq!(init_resp["jsonrpc"], "2.0");
    assert_eq!(init_resp["id"], 1);
    assert!(init_resp["error"].is_null());
    assert_eq!(
        init_resp["result"]["protocolVersion"].as_str().unwrap(),
        "2025-11-25"
    );
    assert_eq!(
        init_resp["result"]["serverInfo"]["name"].as_str().unwrap(),
        "wonk"
    );

    // 2. Send notifications/initialized (notification — no response expected).
    let initialized_notif = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "notifications/initialized"
    });
    send_notification(&mut stdin, &initialized_notif);

    // 3. Send tools/list request.
    let list_req = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/list",
        "params": {}
    });
    let list_resp = send_and_recv(&mut stdin, &mut reader, &list_req);

    assert_eq!(list_resp["id"], 2);
    assert!(list_resp["error"].is_null());
    let tools = list_resp["result"]["tools"].as_array().unwrap();
    // TASK-086 adds wonk_review as the 24th tool.
    assert_eq!(tools.len(), 24);

    let tool_names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert!(tool_names.contains(&"wonk_search"));
    assert!(tool_names.contains(&"wonk_sym"));
    assert!(tool_names.contains(&"wonk_ref"));
    assert!(tool_names.contains(&"wonk_sig"));
    // wonk_ls was merged into wonk_summary
    assert!(tool_names.contains(&"wonk_deps"));
    assert!(tool_names.contains(&"wonk_show"));
    assert!(tool_names.contains(&"wonk_rdeps"));
    assert!(tool_names.contains(&"wonk_status"));
    assert!(tool_names.contains(&"wonk_init"));
    assert!(tool_names.contains(&"wonk_callpath"));
    assert!(tool_names.contains(&"wonk_changes"));
    assert!(tool_names.contains(&"wonk_summary"));
    assert!(tool_names.contains(&"wonk_flows"));
    assert!(tool_names.contains(&"wonk_repos"));

    // 4. Send tools/call for wonk_status.
    let status_req = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 3,
        "method": "tools/call",
        "params": {
            "name": "wonk_status",
            "arguments": {}
        }
    });
    let status_resp = send_and_recv(&mut stdin, &mut reader, &status_req);

    assert_eq!(status_resp["id"], 3);
    assert!(status_resp["error"].is_null());
    let content = status_resp["result"]["content"].as_array().unwrap();
    assert_eq!(content.len(), 1);
    assert_eq!(content[0]["type"], "text");
    // Parse the text content as JSON to verify structure.
    let status_text = content[0]["text"].as_str().unwrap();
    let status: Value = serde_json::from_str(status_text).unwrap();
    assert!(status["indexed"].as_bool().unwrap());

    // 5. Send ping.
    let ping_req = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 4,
        "method": "ping"
    });
    let ping_resp = send_and_recv(&mut stdin, &mut reader, &ping_req);
    assert_eq!(ping_resp["id"], 4);
    assert!(ping_resp["error"].is_null());

    // 6. Close stdin — server should exit cleanly.
    drop(stdin);
    let status = child.wait().unwrap();
    assert!(status.success(), "server exited with status: {status}");
}

// -- degraded embedding-provider paths ---------------------------------------
//
// The MCP tools resolve the query provider with the same contract as
// `wonk ask`: an unreachable configured Ollama degrades to the bundled
// provider with a warning, and a stored foreign vector space errors with
// the re-embed command instead of silently searching the wrong space.

/// Offline command (proxy-pinned, no Ollama) rooted at `dir`.
fn offline_command(bin: &Path, dir: &Path) -> Command {
    let mut command = Command::new(bin);
    command
        .current_dir(dir)
        .env("HTTP_PROXY", "http://127.0.0.1:1")
        .env("HTTPS_PROXY", "http://127.0.0.1:1")
        .env("ALL_PROXY", "http://127.0.0.1:1")
        .env("NO_PROXY", "")
        .env_remove("OLLAMA_HOST");
    command
}

/// Fixture: temp git repo with an indexed `src/auth.rs` (bundled
/// embeddings) in the *central* index location, plus the isolated `$HOME`
/// that central location lives under (`wonk mcp serve` resolves the central
/// index, unlike `wonk ask` which prefers the local one).
///
/// Returns `(repo, home)`; both tempdirs must outlive the server child.
fn indexed_central_repo(bin: &Path) -> (tempfile::TempDir, tempfile::TempDir) {
    let repo = tempfile::tempdir().unwrap();
    Command::new("git")
        .args(["init"])
        .current_dir(repo.path())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    let src_dir = repo.path().join("src");
    fs::create_dir_all(&src_dir).unwrap();
    fs::write(
        src_dir.join("auth.rs"),
        r#"
/// Authenticate a user and create an application session.
pub fn authenticate_user(token: &str) -> Session {
    Session::from_token(token)
}
"#,
    )
    .unwrap();

    let home = tempfile::tempdir().unwrap();
    let init = offline_command(bin, repo.path())
        .env("HOME", home.path())
        .args(["init"])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    assert!(
        init.status.success(),
        "wonk init failed: {}",
        String::from_utf8_lossy(&init.stderr)
    );
    (repo, home)
}

/// The central `index.db` built by [`indexed_central_repo`] under `home`.
fn central_index_path(home: &Path) -> PathBuf {
    let repos_dir = home.join(".wonk").join("repos");
    for entry in fs::read_dir(&repos_dir).expect("central repos dir after init") {
        let index = entry.unwrap().path().join("index.db");
        if index.exists() {
            return index;
        }
    }
    panic!("no central index found under {}", repos_dir.display());
}

/// Rewrite every stored embedding row so it claims the ollama vector space,
/// simulating an index built (or switched) with a different provider.
fn relabel_embeddings_as_ollama(index: &Path) {
    let conn = rusqlite::Connection::open(index).unwrap();
    let changed = conn
        .execute("UPDATE embeddings SET provider = 'ollama', dim = 768", [])
        .unwrap();
    assert!(changed > 0, "expected existing embeddings to relabel");
}

/// Spawn `wonk mcp serve` offline (proxy-pinned) in `repo`, with the
/// isolated `home` holding the central index and the global config.
fn spawn_offline_mcp_server(bin: &Path, repo: &Path, home: &Path) -> Child {
    offline_command(bin, repo)
        .env("HOME", home)
        .args(["mcp", "serve"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn wonk mcp serve")
}

/// Run the initialize handshake and a `wonk_ask` tool call; return the
/// tools/call response plus everything the server wrote to stderr by the
/// time it exited.
fn mcp_wonk_ask(bin: &Path, repo: &Path, home: &Path, query: &str) -> (Value, String) {
    let mut child = spawn_offline_mcp_server(bin, repo, home);
    let mut stdin = child.stdin.take().unwrap();
    let mut reader = BufReader::new(child.stdout.take().unwrap());
    let mut stderr = child.stderr.take().unwrap();

    let init_req = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-11-25",
            "capabilities": {},
            "clientInfo": {"name": "test", "version": "0.1"}
        }
    });
    let init_resp = send_and_recv(&mut stdin, &mut reader, &init_req);
    assert!(
        init_resp["error"].is_null(),
        "initialize failed: {init_resp}"
    );

    send_notification(
        &mut stdin,
        &serde_json::json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
    );

    let ask_req = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/call",
        "params": {
            "name": "wonk_ask",
            "arguments": {"query": query}
        }
    });
    let ask_resp = send_and_recv(&mut stdin, &mut reader, &ask_req);
    assert_eq!(ask_resp["id"], 2, "response id mismatch: {ask_resp}");
    assert!(ask_resp["error"].is_null(), "tools/call failed: {ask_resp}");

    // Close stdin so the server exits, then drain stderr.
    drop(stdin);
    let mut stderr_text = String::new();
    stderr
        .read_to_string(&mut stderr_text)
        .expect("read server stderr");
    let status = child.wait().unwrap();
    assert!(status.success(), "server exited with status: {status}");
    (ask_resp, stderr_text)
}

#[test]
fn mcp_ask_degrades_to_bundled_when_ollama_unreachable() {
    let bin = wonk_bin();
    assert!(bin.exists(), "wonk binary not found at {}", bin.display());
    let (repo, home) = indexed_central_repo(&bin);

    // Configure the (offline) Ollama provider after the bundled index was
    // built, so queries must degrade to bundled.
    fs::create_dir_all(repo.path().join(".wonk")).unwrap();
    fs::write(
        repo.path().join(".wonk/config.toml"),
        "[embedding]\nprovider = \"ollama\"\n",
    )
    .unwrap();

    let (resp, stderr) = mcp_wonk_ask(&bin, repo.path(), home.path(), "authentication");
    assert!(
        resp["result"]["isError"].is_null(),
        "tool_ask should degrade to bundled results, not error: {resp}"
    );
    let text = resp["result"]["content"][0]["text"].as_str().unwrap_or("");
    assert!(
        text.contains("authenticate_user"),
        "expected bundled-space results in the tool output, got: {text}"
    );
    assert!(
        stderr.contains("falling back to the bundled provider"),
        "expected the fallback warning on the server's stderr, got: {stderr}"
    );
}

#[test]
fn mcp_ask_blocks_on_provider_switch_with_reembed_command() {
    let bin = wonk_bin();
    assert!(bin.exists(), "wonk binary not found at {}", bin.display());
    let (repo, home) = indexed_central_repo(&bin);
    relabel_embeddings_as_ollama(&central_index_path(home.path()));

    fs::create_dir_all(repo.path().join(".wonk")).unwrap();
    fs::write(
        repo.path().join(".wonk/config.toml"),
        "[embedding]\nprovider = \"ollama\"\n",
    )
    .unwrap();

    let (resp, _stderr) = mcp_wonk_ask(&bin, repo.path(), home.path(), "authentication");
    assert_eq!(
        resp["result"]["isError"], true,
        "tool_ask must refuse a provider switch, got: {resp}"
    );
    let text = resp["result"]["content"][0]["text"].as_str().unwrap_or("");
    assert!(
        text.contains("vector space mismatch"),
        "expected mismatch error in the tool result, got: {text}"
    );
    assert!(
        text.contains("active bundled/256"),
        "expected the bundled fallback space in the error, got: {text}"
    );
    assert!(
        text.contains("stored ollama/768"),
        "expected the stored ollama space in the error, got: {text}"
    );
    assert!(
        text.contains("wonk update --force --provider bundled"),
        "expected re-embed command in the tool result, got: {text}"
    );
}

// -- BM25-ranked lexical input to wonk_ask fusion (TASK-079) ------------------
//
// PRD-BM25-REQ-004: when hybrid fusion runs, the lexical list entering RRF
// must be BM25-ranked, on every production `fuse_rrf` call site — including
// the MCP `wonk_ask` tool, not just the CLI `--semantic` path.

/// Fixture like [`indexed_central_repo`], but with a corpus that separates
/// BM25 order from match-presence (walk) order: four files, all six lines
/// long, mentioning "gewgaw" 1 / 4 / 8 / 12 times on comment lines only (so
/// no grep match line coincides with a symbol line and fusion never boosts a
/// lexical entry semantically). BM25 must order the files by saturation:
/// saturated, lots, some, once.
fn indexed_central_repo_bm25(bin: &Path) -> (tempfile::TempDir, tempfile::TempDir) {
    let repo = tempfile::tempdir().unwrap();
    Command::new("git")
        .args(["init"])
        .current_dir(repo.path())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    let src_dir = repo.path().join("src");
    fs::create_dir_all(&src_dir).unwrap();
    fs::write(
        src_dir.join("once.rs"),
        concat!(
            "// gewgaw\n",
            "pub fn once_probe() -> u32 {\n",
            "    1\n",
            "}\n",
            "// end of once\n",
            "// tail marker\n",
        ),
    )
    .unwrap();
    fs::write(
        src_dir.join("some.rs"),
        concat!(
            "// gewgaw alpha and gewgaw beta.\n",
            "// gewgaw gamma plus gewgaw delta.\n",
            "pub fn some_probe() -> u32 {\n",
            "    2\n",
            "}\n",
            "// tail marker\n",
        ),
    )
    .unwrap();
    fs::write(
        src_dir.join("lots.rs"),
        concat!(
            "// gewgaw a, gewgaw b, gewgaw c.\n",
            "// gewgaw d, gewgaw e, gewgaw f.\n",
            "// gewgaw g plus gewgaw h complete.\n",
            "pub fn lots_probe() -> u32 {\n",
            "    3\n",
            "}\n",
        ),
    )
    .unwrap();
    fs::write(
        src_dir.join("saturated.rs"),
        concat!(
            "// gewgaw gewgaw gewgaw gewgaw alpha.\n",
            "// gewgaw gewgaw gewgaw gewgaw beta.\n",
            "// gewgaw gewgaw gewgaw gewgaw gamma.\n",
            "pub fn saturated_probe() -> u32 {\n",
            "    4\n",
            "}\n",
        ),
    )
    .unwrap();

    let home = tempfile::tempdir().unwrap();
    let init = offline_command(bin, repo.path())
        .env("HOME", home.path())
        .args(["init"])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    assert!(
        init.status.success(),
        "wonk init failed: {}",
        String::from_utf8_lossy(&init.stderr)
    );
    (repo, home)
}

#[test]
fn mcp_ask_fuses_bm25_ranked_lexical_input() {
    let bin = wonk_bin();
    assert!(bin.exists(), "wonk binary not found at {}", bin.display());
    let (repo, home) = indexed_central_repo_bm25(&bin);

    let (resp, _stderr) = mcp_wonk_ask(&bin, repo.path(), home.path(), "gewgaw");
    assert!(
        resp["result"]["isError"].is_null(),
        "tool_ask should fuse results, not error: {resp}"
    );
    let text = resp["result"]["content"][0]["text"].as_str().unwrap();
    let outputs: Vec<Value> =
        serde_json::from_str(text).expect("structural branch returns a JSON array");

    // Grep-backed entries carry no annotation; semantic-only entries are
    // annotated "[semantic: ...]". Restrict to the grep-backed ones so the
    // assertion observes the lexical list's fusion order.
    let mut seen: Vec<String> = Vec::new();
    for out in &outputs {
        if out["annotation"].is_null()
            && let Some(file) = out["file"].as_str()
            && !seen.iter().any(|s| file.ends_with(s))
        {
            seen.push(file.rsplit('/').next().unwrap_or(file).to_string());
        }
    }
    assert_eq!(
        seen,
        vec![
            "saturated.rs".to_string(),
            "lots.rs".to_string(),
            "some.rs".to_string(),
            "once.rs".to_string(),
        ],
        "PRD-BM25-REQ-004: wonk_ask must feed fuse_rrf the BM25-ranked list \
         (descending term saturation), not the raw grep walk order; got [{}] \
         in {text}",
        seen.join(", ")
    );
}
