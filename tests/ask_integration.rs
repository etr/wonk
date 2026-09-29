//! Integration tests for `wonk ask` (semantic search).
//!
//! The default-provider test is fully offline. The opt-in Ollama test remains
//! ignored so CI does not require an external service.

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

fn offline_command(bin: &std::path::Path, repo: &std::path::Path) -> Command {
    let mut command = Command::new(bin);
    command
        .current_dir(repo)
        .env("HTTP_PROXY", "http://127.0.0.1:1")
        .env("HTTPS_PROXY", "http://127.0.0.1:1")
        .env("ALL_PROXY", "http://127.0.0.1:1")
        .env("NO_PROXY", "")
        .env_remove("OLLAMA_HOST");
    command
}

#[test]
fn ask_works_with_bundled_default_offline() {
    let bin = wonk_bin();
    assert!(bin.exists(), "wonk binary not found at {}", bin.display());

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
/// Authenticate a user and create an application session.
pub fn authenticate_user(token: &str) -> Session {
    Session::from_token(token)
}

pub struct Session;
impl Session {
    fn from_token(_token: &str) -> Self { Self }
}
"#,
    )
    .unwrap();
    fs::write(
        src_dir.join("sorting.rs"),
        "pub fn quicksort(values: &mut [i32]) { values.sort(); }\n",
    )
    .unwrap();

    let init = offline_command(&bin, tmp.path())
        .args(["init", "--local"])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    assert!(
        init.status.success(),
        "wonk init failed: {}",
        String::from_utf8_lossy(&init.stderr)
    );

    let output = offline_command(&bin, tmp.path())
        .args(["ask", "authentication", "--format", "json"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "wonk ask failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(!stdout.trim().is_empty(), "expected non-empty JSON output");
    let results: Vec<Value> = stdout
        .split(" ; ")
        .map(|record| {
            serde_json::from_str(record).unwrap_or_else(|error| {
                panic!("valid JSON result ({error}): {record}");
            })
        })
        .collect();
    assert!(
        results
            .iter()
            .any(|result| result["symbol_name"] == "authenticate_user"),
        "{results:?}"
    );

    let conn = rusqlite::Connection::open(tmp.path().join(".wonk/index.db")).unwrap();
    let metadata: (String, i64) = conn
        .query_row("SELECT provider, dim FROM embeddings LIMIT 1", [], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })
        .unwrap();
    assert_eq!(metadata, ("bundled".to_string(), 256));
}

/// Fixture: temp git repo with an indexed `src/auth.rs` (bundled embeddings).
fn indexed_bundled_repo(bin: &std::path::Path) -> tempfile::TempDir {
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
/// Authenticate a user and create an application session.
pub fn authenticate_user(token: &str) -> Session {
    Session::from_token(token)
}
"#,
    )
    .unwrap();

    let init = offline_command(bin, tmp.path())
        .args(["init", "--local"])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    assert!(
        init.status.success(),
        "wonk init failed: {}",
        String::from_utf8_lossy(&init.stderr)
    );
    tmp
}

/// Rewrite every stored embedding row so it claims the ollama vector space,
/// simulating an index built (or switched) with a different provider.
fn relabel_embeddings_as_ollama(repo: &std::path::Path) {
    let conn = rusqlite::Connection::open(repo.join(".wonk/index.db")).unwrap();
    let changed = conn
        .execute("UPDATE embeddings SET provider = 'ollama', dim = 768", [])
        .unwrap();
    assert!(changed > 0, "expected existing embeddings to relabel");
}

#[test]
fn ask_falls_back_to_bundled_when_ollama_unreachable() {
    let bin = wonk_bin();
    assert!(bin.exists(), "wonk binary not found at {}", bin.display());
    let tmp = indexed_bundled_repo(&bin);

    fs::create_dir_all(tmp.path().join(".wonk")).unwrap();
    fs::write(
        tmp.path().join(".wonk/config.toml"),
        "[embedding]\nprovider = \"ollama\"\n",
    )
    .unwrap();

    let output = offline_command(&bin, tmp.path())
        .args(["ask", "authentication", "--format", "json"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "wonk ask should degrade, not fail: {stderr}"
    );
    assert!(
        stderr.contains("falling back to the bundled provider"),
        "expected fallback warning on stderr, got: {stderr}"
    );

    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        stdout.contains("authenticate_user"),
        "expected bundled-space results on stdout, got: {stdout}"
    );

    let conn = rusqlite::Connection::open(tmp.path().join(".wonk/index.db")).unwrap();
    let metadata: (String, i64) = conn
        .query_row("SELECT provider, dim FROM embeddings LIMIT 1", [], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })
        .unwrap();
    assert_eq!(metadata, ("bundled".to_string(), 256));
}

#[test]
fn ask_blocks_on_provider_switch_with_reembed_command() {
    let bin = wonk_bin();
    assert!(bin.exists(), "wonk binary not found at {}", bin.display());
    let tmp = indexed_bundled_repo(&bin);
    relabel_embeddings_as_ollama(tmp.path());

    let output = offline_command(&bin, tmp.path())
        .args(["ask", "authentication", "--format", "json"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    assert!(
        !output.status.success(),
        "provider switch must refuse, not silently search or re-embed"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("vector space mismatch"),
        "expected mismatch error, got: {stderr}"
    );
    assert!(
        stderr.contains("wonk update --force --provider bundled"),
        "expected re-embed command, got: {stderr}"
    );
    assert!(
        output.stdout.is_empty(),
        "no structured results on a blocked query, got: {}",
        String::from_utf8_lossy(&output.stdout)
    );
}

#[test]
fn ask_unreachable_ollama_with_stored_ollama_vectors_blocks() {
    let bin = wonk_bin();
    assert!(bin.exists(), "wonk binary not found at {}", bin.display());
    let tmp = indexed_bundled_repo(&bin);
    relabel_embeddings_as_ollama(tmp.path());
    fs::create_dir_all(tmp.path().join(".wonk")).unwrap();
    fs::write(
        tmp.path().join(".wonk/config.toml"),
        "[embedding]\nprovider = \"ollama\"\n",
    )
    .unwrap();

    let output = offline_command(&bin, tmp.path())
        .args(["ask", "authentication", "--format", "json"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    assert!(
        !output.status.success(),
        "fallback must never cross a mismatched vector space"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("vector space mismatch"),
        "expected mismatch error, got: {stderr}"
    );
    assert!(
        stderr.contains("--provider bundled"),
        "expected re-embed command, got: {stderr}"
    );
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
    fs::create_dir_all(tmp.path().join(".wonk")).unwrap();
    fs::write(
        tmp.path().join(".wonk/config.toml"),
        "[embedding]\nprovider = \"ollama\"\n",
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
        .arg("init")
        .current_dir(tmp.path())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .status()
        .unwrap();
    assert!(init.success(), "wonk init with configured Ollama failed");

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
    for record in stdout.split(" ; ") {
        let v: Value = serde_json::from_str(record).expect("each record should be valid JSON");
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

#[test]
fn search_semantic_falls_back_to_bundled_when_ollama_unreachable() {
    let bin = wonk_bin();
    assert!(bin.exists(), "wonk binary not found at {}", bin.display());
    let tmp = indexed_bundled_repo(&bin);

    fs::create_dir_all(tmp.path().join(".wonk")).unwrap();
    fs::write(
        tmp.path().join(".wonk/config.toml"),
        "[embedding]\nprovider = \"ollama\"\n",
    )
    .unwrap();

    let output = offline_command(&bin, tmp.path())
        .args(["search", "--semantic", "authentication", "--format", "json"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "wonk search --semantic should degrade, not fail: {stderr}"
    );
    assert!(
        stderr.contains("falling back to the bundled provider"),
        "expected fallback warning on stderr, got: {stderr}"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("auth.rs"),
        "expected blended results on stdout, got: {stdout}"
    );
}

#[test]
fn status_reports_provider_and_dimension() {
    let bin = wonk_bin();
    assert!(bin.exists(), "wonk binary not found at {}", bin.display());
    let tmp = indexed_bundled_repo(&bin);

    let output = offline_command(&bin, tmp.path())
        .args(["status", "--format", "json"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "wonk status failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let info: Value = serde_json::from_slice(&output.stdout).expect("status JSON should parse");
    assert_eq!(info["active_provider"], "bundled");
    assert_eq!(info["stored_vector_provider"], "bundled");
    assert_eq!(info["stored_vector_dim"], 256);
    assert_eq!(info["ollama_reachable"], Value::Null);

    // Configured Ollama + offline: the probe reports unreachable and the
    // human-readable output names the fallback.
    fs::create_dir_all(tmp.path().join(".wonk")).unwrap();
    fs::write(
        tmp.path().join(".wonk/config.toml"),
        "[embedding]\nprovider = \"ollama\"\n",
    )
    .unwrap();

    let json_output = offline_command(&bin, tmp.path())
        .args(["status", "--format", "json"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    assert!(json_output.status.success());
    let info: Value = serde_json::from_slice(&json_output.stdout).unwrap();
    assert_eq!(info["active_provider"], "ollama");
    assert_eq!(info["ollama_reachable"], false);

    let human_output = offline_command(&bin, tmp.path())
        .args(["status"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    assert!(human_output.status.success());
    let stderr = String::from_utf8_lossy(&human_output.stderr);
    assert!(
        stderr.contains("Ollama: unreachable — semantic queries fall back to the bundled provider"),
        "expected fallback note in status output, got: {stderr}"
    );
}
