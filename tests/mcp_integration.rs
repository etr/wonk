//! Integration tests for the MCP server (`wonk mcp serve`).
//!
//! Spawns the server as a subprocess with piped stdin/stdout and verifies
//! the JSON-RPC handshake and tool listing.

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

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
    assert_eq!(tools.len(), 9);

    let tool_names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert!(tool_names.contains(&"wonk_search"));
    assert!(tool_names.contains(&"wonk_sym"));
    assert!(tool_names.contains(&"wonk_ref"));
    assert!(tool_names.contains(&"wonk_sig"));
    assert!(tool_names.contains(&"wonk_ls"));
    assert!(tool_names.contains(&"wonk_deps"));
    assert!(tool_names.contains(&"wonk_rdeps"));
    assert!(tool_names.contains(&"wonk_status"));
    assert!(tool_names.contains(&"wonk_init"));

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
