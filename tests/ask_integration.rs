//! Integration tests for `wonk ask` (semantic search).
//!
//! Requires a running Ollama server with the `nomic-embed-text` model pulled.
//! Marked `#[ignore]` so CI does not fail without Ollama.

use std::fs;
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

#[test]
#[ignore]
fn ask_returns_json_results() {
    let bin = wonk_bin();
    if !bin.exists() {
        panic!("wonk binary not found at {}", bin.display());
    }

    // Create a temp git repo with a source file.
    let tmp = tempfile::tempdir().unwrap();
    Command::new("git")
        .args(["init"])
        .current_dir(tmp.path())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();

    let src_dir = tmp.path().join("src");
    fs::create_dir_all(&src_dir).unwrap();
    fs::write(
        src_dir.join("auth.rs"),
        r#"
/// Authenticate the user with username and password.
fn authenticate(username: &str, password: &str) -> bool {
    username == "admin" && password == "secret"
}

/// Check if the session token is valid.
fn validate_token(token: &str) -> bool {
    !token.is_empty()
}
"#,
    )
    .unwrap();

    // Stage + commit so wonk sees the files.
    Command::new("git")
        .args(["add", "."])
        .current_dir(tmp.path())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    Command::new("git")
        .args(["commit", "-m", "init"])
        .current_dir(tmp.path())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();

    // Build index with embeddings.
    let init = Command::new(&bin)
        .args(["init", "--embed"])
        .current_dir(tmp.path())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .status()
        .unwrap();
    assert!(init.success(), "wonk init --embed failed");

    // Run `wonk ask` with JSON output.
    let output = Command::new(&bin)
        .args(["ask", "authentication", "--format", "json"])
        .current_dir(tmp.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    assert!(output.status.success(), "wonk ask failed");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(!stdout.trim().is_empty(), "expected non-empty output");

    // Each line should be a valid JSON object with expected fields.
    for line in stdout.lines() {
        let v: Value = serde_json::from_str(line).expect("each line should be valid JSON");
        assert!(v.get("file").is_some(), "missing 'file' field");
        assert!(v.get("line").is_some(), "missing 'line' field");
        assert!(
            v.get("symbol_name").is_some(),
            "missing 'symbol_name' field"
        );
        assert!(
            v.get("symbol_kind").is_some(),
            "missing 'symbol_kind' field"
        );
        assert!(
            v.get("similarity_score").is_some(),
            "missing 'similarity_score' field"
        );
    }
}
