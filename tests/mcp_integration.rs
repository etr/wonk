//! Integration tests for the MCP server (`wonk mcp serve`).
//!
//! Spawns the server as a subprocess with piped stdin/stdout and verifies
//! the JSON-RPC handshake and tool listing. The degraded embedding-provider
//! tests additionally pin the child process's proxy environment (as in
//! tests/ask_integration.rs) so the configured Ollama is deterministically
//! unreachable.

mod common;

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

    let mut child = common::command(&bin, tmp.path())
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
    // TASK-101 adds wonk_feedback as the 25th tool.
    assert_eq!(tools.len(), 25);

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
    let mut command = common::command(bin, dir);
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

// -- rank-config-gated wonk_search (TASK-092) --------------------------------
//
// The rewired tool_search loads config per call: [rank.weights] is
// validated against the signal registry (unknown names are a hard error)
// and config.rank.enabled gates the rerank pipeline. Rows must stay
// identical either way under the default kind-only weights (AR-033).

/// Fixture like [`indexed_central_repo`], with a corpus whose
/// "authenticate_user" hits produce a deterministic ranked row set: an
/// import line, a definition-looking line, and a call line. (The MCP
/// search path passes absolute roots, so index-based Definition/CallSite
/// classification does not fire — rows come from the content heuristics,
/// exactly as before the rerank rewire.)
fn indexed_central_repo_search(bin: &Path) -> (tempfile::TempDir, tempfile::TempDir) {
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
        "use crate::session::Session;\n\npub fn authenticate_user(token: &str) -> Session {\n    Session::from_token(token)\n}\n",
    )
    .unwrap();
    fs::write(
        src_dir.join("caller.rs"),
        "use crate::auth::authenticate_user;\n\npub fn login() {\n    authenticate_user(\"t\");\n}\n",
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

/// One live MCP server session with the handshake completed, ready for
/// tool calls. `finish` closes stdin, drains stderr, and asserts a clean
/// exit.
struct McpSession {
    child: Child,
    stdin: Box<dyn Write>,
    reader: Box<dyn BufRead>,
}

impl McpSession {
    fn start(bin: &Path, repo: &Path, home: &Path) -> Self {
        let mut child = spawn_offline_mcp_server(bin, repo, home);
        let mut stdin: Box<dyn Write> = Box::new(child.stdin.take().unwrap());
        let mut reader: Box<dyn BufRead> = Box::new(BufReader::new(child.stdout.take().unwrap()));

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
        Self {
            child,
            stdin,
            reader,
        }
    }

    fn wonk_search_with_args(&mut self, arguments: Value) -> Value {
        let req = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {
                "name": "wonk_search",
                "arguments": arguments
            }
        });
        send_and_recv(&mut self.stdin, &mut self.reader, &req)
    }

    fn wonk_search(&mut self, query: &str) -> Value {
        let req = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {
                "name": "wonk_search",
                "arguments": {"query": query}
            }
        });
        send_and_recv(&mut self.stdin, &mut self.reader, &req)
    }

    fn finish(mut self) {
        // Close stdin so the server exits; keep the stdout reader open
        // through wait() (dropping it early EPIPEs the server's final
        // writes), mirroring the mcp_wonk_ask close sequence.
        drop(self.stdin);
        let mut stderr_text = String::new();
        if let Some(mut stderr) = self.child.stderr.take() {
            let _ = stderr.read_to_string(&mut stderr_text);
        }
        let status = self.child.wait().unwrap();
        assert!(
            status.success(),
            "server exited with status {status} (stderr: {stderr_text})"
        );
        drop(self.reader);
    }
}

#[test]
fn mcp_search_rows_stable_from_default_through_enabled_pipeline() {
    let bin = wonk_bin();
    assert!(bin.exists(), "wonk binary not found at {}", bin.display());
    let (repo, home) = indexed_central_repo_search(&bin);

    // Default config (pipeline disabled, REQ-017): rows render in the
    // pre-change format — tier-ordered (import first, then the remaining
    // lines by (file, line)), each row carrying exactly file/line/col/
    // content.
    let mut session = McpSession::start(&bin, repo.path(), home.path());
    let default_resp = session.wonk_search("authenticate_user");
    assert!(
        default_resp["result"]["isError"].is_null(),
        "default config must keep wonk_search working: {default_resp}"
    );
    let default_text = default_resp["result"]["content"][0]["text"]
        .as_str()
        .unwrap();
    let rows: Vec<Value> =
        serde_json::from_str(default_text).expect("wonk_search returns a JSON row array");

    // TASK-105's canonical file keys make the absolute-path MCP results
    // classify like their CLI equivalents: the re-export import now merges
    // into the definition (annotation) instead of standing apart.
    let expected: [(&str, u64, &str, bool); 2] = [
        (
            "src/auth.rs",
            3,
            "pub fn authenticate_user(token: &str) -> Session {",
            true,
        ),
        ("src/caller.rs", 4, "    authenticate_user(\"t\");", false),
    ];
    assert_eq!(rows.len(), expected.len(), "row set: {default_text}");
    for (row, (file, line, content, deduped)) in rows.iter().zip(&expected) {
        assert_eq!(row["line"], *line, "row line mismatch: {row}");
        assert_eq!(row["col"], 1, "row col mismatch: {row}");
        assert_eq!(
            row["content"].as_str().unwrap(),
            *content,
            "row content mismatch: {row}"
        );
        let got_file = row["file"].as_str().unwrap();
        assert!(
            got_file.ends_with(file),
            "row file {got_file} must end with {file}"
        );
        // The annotation field renders only when dedup fires; it must
        // stay absent otherwise (pre-change serde shape).
        assert_eq!(
            row.get("annotation").and_then(Value::as_str),
            if *deduped {
                Some("(+1 other location)")
            } else {
                None
            },
            "annotation must track dedup: {row}"
        );
    }

    // The mid-session flip repurposed post-TASK-095: an explicit
    // enabled=true is byte-identical to the (now-pipelined) default...
    fs::create_dir_all(repo.path().join(".wonk")).unwrap();
    fs::write(
        repo.path().join(".wonk/config.toml"),
        "[rank]\nenabled = true\n",
    )
    .unwrap();
    let enabled_resp = session.wonk_search("authenticate_user");
    assert!(
        enabled_resp["result"]["isError"].is_null(),
        "enabled pipeline must keep wonk_search working: {enabled_resp}"
    );
    let enabled_text = enabled_resp["result"]["content"][0]["text"]
        .as_str()
        .unwrap();
    assert_eq!(
        default_text, enabled_text,
        "explicit enabled=true must reproduce the flipped default byte-for-byte"
    );

    // ...and the escape hatch: flipping enabled=false mid-session returns
    // to the legacy path — same rows, minus the recorded query_class.
    fs::write(
        repo.path().join(".wonk/config.toml"),
        "[rank]\nenabled = false\n",
    )
    .unwrap();
    let disabled_resp = session.wonk_search("authenticate_user");
    let disabled_text = disabled_resp["result"]["content"][0]["text"]
        .as_str()
        .unwrap();
    let disabled_rows: Vec<Value> = serde_json::from_str(disabled_text).unwrap();
    assert_eq!(
        disabled_rows.len(),
        rows.len(),
        "the legacy path must not change the row count"
    );
    for (legacy, pipelined) in disabled_rows.iter().zip(&rows) {
        assert_eq!(legacy["file"], pipelined["file"], "ordering must hold");
        assert_eq!(legacy["line"], pipelined["line"], "ordering must hold");
        assert_eq!(legacy["content"], pipelined["content"], "content must hold");
        assert!(
            legacy.get("query_class").is_none(),
            "legacy rows carry no class: {legacy}"
        );
    }
    session.finish();
}

#[test]
fn mcp_search_query_class_pin_and_validation() {
    let bin = wonk_bin();
    assert!(bin.exists(), "wonk binary not found at {}", bin.display());
    let (repo, home) = indexed_central_repo_search(&bin);
    let mut session = McpSession::start(&bin, repo.path(), home.path());

    // The tools/list schema advertises the enum.
    let list_req = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 9,
        "method": "tools/list"
    });
    let list = send_and_recv(&mut session.stdin, &mut session.reader, &list_req);
    let tools = list["result"]["tools"].as_array().expect("tools array");
    let search = tools
        .iter()
        .find(|t| t["name"] == "wonk_search")
        .expect("wonk_search tool");
    let prop = &search["inputSchema"]["properties"]["query_class"];
    assert_eq!(
        prop["type"], "string",
        "query_class is a string enum: {prop}"
    );
    let variants: Vec<&str> = prop["enum"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(
        variants,
        vec!["symbol", "path", "signature", "conceptual"],
        "query_class enum must list every class"
    );

    // A valid pin is accepted (default config: rows unchanged, no error).
    let pinned = session.wonk_search_with_args(serde_json::json!({
        "query": "authenticate_user",
        "query_class": "symbol"
    }));
    assert!(
        pinned["result"]["isError"].is_null(),
        "a valid query_class pin must not fail: {pinned}"
    );

    // An invalid pin is a tool error naming the valid values.
    let invalid = session.wonk_search_with_args(serde_json::json!({
        "query": "authenticate_user",
        "query_class": "troll"
    }));
    assert_eq!(
        invalid["result"]["isError"], true,
        "invalid query_class must fail the tool call: {invalid}"
    );
    let text = invalid["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or("");
    for valid in ["symbol", "path", "signature", "conceptual"] {
        assert!(text.contains(valid), "error must name {valid}: {text}");
    }
    session.finish();
}

#[test]
fn mcp_search_rows_record_query_class_when_pipelined() {
    let bin = wonk_bin();
    assert!(bin.exists(), "wonk binary not found at {}", bin.display());
    let (repo, home) = indexed_central_repo_search(&bin);
    let mut session = McpSession::start(&bin, repo.path(), home.path());

    // Post-flip default: every row records the detected class...
    let piped_resp = session.wonk_search("authenticate_user");
    let piped_text = piped_resp["result"]["content"][0]["text"].as_str().unwrap();
    let piped_rows: Vec<Value> = serde_json::from_str(piped_text).unwrap();
    assert!(!piped_rows.is_empty());
    for row in &piped_rows {
        assert_eq!(
            row["query_class"], "symbol",
            "default (flipped) rows record the class: {row}"
        );
    }

    // ...and a PIN is echoed verbatim on every row.
    let pinned_resp = session.wonk_search_with_args(serde_json::json!({
        "query": "authenticate_user",
        "query_class": "conceptual"
    }));
    let pinned_text = pinned_resp["result"]["content"][0]["text"]
        .as_str()
        .unwrap();
    let pinned_rows: Vec<Value> = serde_json::from_str(pinned_text).unwrap();
    assert!(!pinned_rows.is_empty());
    for row in &pinned_rows {
        assert_eq!(
            row["query_class"], "conceptual",
            "the pin is echoed on the rows: {row}"
        );
    }

    // The escape hatch: explicit enabled = false returns to legacy rows
    // carrying no query_class key at all.
    fs::create_dir_all(repo.path().join(".wonk")).unwrap();
    fs::write(
        repo.path().join(".wonk/config.toml"),
        "[rank]\nenabled = false\n",
    )
    .unwrap();
    let legacy_resp = session.wonk_search("authenticate_user");
    let legacy_text = legacy_resp["result"]["content"][0]["text"]
        .as_str()
        .unwrap();
    let legacy_rows: Vec<Value> = serde_json::from_str(legacy_text).unwrap();
    assert_eq!(legacy_rows.len(), piped_rows.len());
    for row in &legacy_rows {
        assert!(
            row.get("query_class").is_none(),
            "legacy MCP rows carry no class: {row}"
        );
    }
    session.finish();
}

#[test]
fn mcp_search_invalid_rank_weights_is_tool_error() {
    let bin = wonk_bin();
    assert!(bin.exists(), "wonk binary not found at {}", bin.display());
    let (repo, home) = indexed_central_repo_search(&bin);

    // Start the server while the config is still valid, then poison
    // [rank.weights]: the handler's per-call config load rejects unknown
    // signal names, surfacing as an isError CallToolResult naming the
    // offender (a server started with this config would exit earlier, in
    // CLI dispatch).
    let mut session = McpSession::start(&bin, repo.path(), home.path());
    fs::create_dir_all(repo.path().join(".wonk")).unwrap();
    fs::write(
        repo.path().join(".wonk/config.toml"),
        "[rank.weights]\nbogus = 1.0\n",
    )
    .unwrap();
    let resp = session.wonk_search("authenticate_user");
    assert_eq!(
        resp["result"]["isError"], true,
        "unknown signal name must fail the tool call: {resp}"
    );
    let text = resp["result"]["content"][0]["text"].as_str().unwrap_or("");
    assert!(
        text.contains("unknown signal name 'bogus'"),
        "error must name the offending signal: {text}"
    );
    assert!(
        text.contains("known: kind"),
        "error must list the valid signal names: {text}"
    );
    session.finish();
}

#[test]
fn mcp_show_malformed_source_keeps_raw_payload_and_warns() {
    let bin = wonk_bin();
    let repo = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    std::fs::create_dir(repo.path().join(".git")).unwrap();
    std::fs::write(
        repo.path().join("target.rs"),
        "pub fn target() {\n    let value = ;\n    work();\n}\n",
    )
    .unwrap();
    let init = offline_command(&bin, repo.path())
        .env("HOME", home.path())
        .args(["init"])
        .output()
        .unwrap();
    assert!(
        init.status.success(),
        "{}",
        String::from_utf8_lossy(&init.stderr)
    );
    let mut child = spawn_offline_mcp_server(&bin, repo.path(), home.path());
    let mut stdin = child.stdin.take().unwrap();
    let mut reader = BufReader::new(child.stdout.take().unwrap());
    let init = send_and_recv(
        &mut stdin,
        &mut reader,
        &serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"fixture","version":"1"}}}),
    );
    assert!(init["error"].is_null(), "{init}");
    let raw = send_and_recv(
        &mut stdin,
        &mut reader,
        &serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"wonk_show","arguments":{"name":"target"}}}),
    );
    let fallback = send_and_recv(
        &mut stdin,
        &mut reader,
        &serde_json::json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"wonk_show","arguments":{"name":"target","elide":"bodies"}}}),
    );
    assert_ne!(raw["result"]["isError"], true, "{raw}");
    assert_ne!(fallback["result"]["isError"], true, "{fallback}");
    assert_eq!(
        fallback["result"]["content"], raw["result"]["content"],
        "MCP source payload must remain byte-identical"
    );
    assert!(
        fallback["result"]["content"]
            .to_string()
            .contains("let value = ;"),
        "{fallback}"
    );
    drop(stdin);
    assert!(child.wait().unwrap().success());
    let mut warning = String::new();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut warning)
        .unwrap();
    assert!(
        warning.contains("elision not applied") && warning.contains("parse failure"),
        "{warning}"
    );
}
