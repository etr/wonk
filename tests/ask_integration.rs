//! Integration tests for `wonk ask` (semantic search).
//!
//! The default-provider test is fully offline. The opt-in Ollama test remains
//! ignored so CI does not require an external service.

use std::fs;
use std::io::{Read as _, Write as _};
use std::net::{SocketAddr, TcpListener};
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
    // NDJSON: one JSON object per line, no " ; " joining on the piped path.
    let results: Vec<Value> = stdout
        .trim_end()
        .split('\n')
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

    // Each line should be a valid JSON object with expected fields (NDJSON:
    // one object per line, no " ; " joining on the piped path).
    for record in stdout.trim_end().split('\n') {
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

// -- mid-session disconnect (mock Ollama) -----------------------------------
//
// The plan-time degraded paths above pin the probe-failing direction. The
// two tests below pin the race the mid-embed fallback exists for: the probe
// succeeds, then the embed endpoint dies.

/// A stand-in Ollama that is stopped mid-session: it answers exactly one
/// health probe (`GET /` → 200 OK), then stops listening, so every later
/// connection — the `/api/embed` the query needs — is refused. The client
/// reports that as `ConnectionFailed`, which the embedding layer classifies
/// as `EmbeddingError::OllamaUnreachable`: "Ollama was healthy at plan time
/// and is gone by embed time".
///
/// The binary hardcodes `http://localhost:11434` for the embedding provider,
/// so the mock impersonates Ollama by acting as the child process's HTTP
/// proxy: ureq routes even localhost through HTTP_PROXY when NO_PROXY is
/// empty and establishes the proxy tunnel with `CONNECT`, after which the
/// real `GET /` request arrives on the same connection.
fn spawn_mock_ollama_stopped_after_the_probe() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock ollama");
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        // Serve the plan-time probe, then die.
        for stream in listener.incoming() {
            let mut stream = match stream {
                Ok(s) => s,
                Err(_) => continue,
            };
            let mut method = request_method(&read_request_head(&mut stream));
            if method == "CONNECT" {
                // Complete the proxy tunnel handshake.
                let _ = stream.write_all(b"HTTP/1.1 200 Connection established\r\n\r\n");
                let _ = stream.flush();
                method = request_method(&read_request_head(&mut stream));
            }
            if method == "GET" {
                // Health probe: the server is up (for now).
                let _ = stream.write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nOK",
                );
                let _ = stream.flush();
            }
            // Stop Ollama: nothing further is accepted or answered.
            drop(listener);
            break;
        }
    });
    addr
}

/// Read from `stream` until the end of the request headers (or EOF/error).
fn read_request_head(stream: &mut std::net::TcpStream) -> Vec<u8> {
    let mut request = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                request.extend_from_slice(&chunk[..n]);
                if request.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    request
}

/// First token of a raw HTTP request head (the method).
fn request_method(request: &[u8]) -> String {
    request
        .split(|&b| b == b' ')
        .next()
        .map(|s| String::from_utf8_lossy(s).into_owned())
        .unwrap_or_default()
}

/// Offline command whose ollama traffic is routed through the mock.
fn mock_routed_command(
    bin: &std::path::Path,
    repo: &std::path::Path,
    proxy: SocketAddr,
) -> Command {
    let mut command = Command::new(bin);
    let proxy = format!("http://{proxy}");
    command
        .current_dir(repo)
        .env("HTTP_PROXY", &proxy)
        .env("HTTPS_PROXY", &proxy)
        .env("ALL_PROXY", &proxy)
        .env("NO_PROXY", "")
        .env_remove("OLLAMA_HOST");
    command
}

/// Mid-session disconnect through the embedding build: the plan-time probe
/// succeeds (Active: ollama), then Ollama is stopped, so the build's next
/// contact with it fails. The build failure must degrade to the bundled
/// provider with the fallback warning instead of failing the query.
///
/// This path cannot produce results on stdout: results would require stored
/// bundled vectors, and a *healthy* configured Ollama over bundled vectors is
/// a provider switch that blocks at plan time (pinned by
/// `decide_ollama_healthy_after_switch_to_bundled_index_blocks`). Degrade
/// with results only happens when the probe itself fails — pinned by
/// `ask_falls_back_to_bundled_when_ollama_unreachable`. Here the vectors are
/// dropped, so after degrading there is nothing to search yet: `wonk ask`
/// exits 0 with a hint and clean stdout.
#[test]
fn ask_degrades_to_bundled_when_ollama_dies_mid_build() {
    let bin = wonk_bin();
    assert!(bin.exists(), "wonk binary not found at {}", bin.display());
    let tmp = indexed_bundled_repo(&bin);

    // Drop every stored vector but keep the symbols: the plan resolves
    // against an empty table (Active: ollama), and `wonk ask` must rebuild
    // the missing embeddings through the configured provider — which the
    // mock has stopped by then.
    let conn = rusqlite::Connection::open(tmp.path().join(".wonk/index.db")).unwrap();
    conn.execute("DELETE FROM embeddings", []).unwrap();
    drop(conn);

    fs::create_dir_all(tmp.path().join(".wonk")).unwrap();
    fs::write(
        tmp.path().join(".wonk/config.toml"),
        "[embedding]\nprovider = \"ollama\"\n",
    )
    .unwrap();

    let mock = spawn_mock_ollama_stopped_after_the_probe();
    let output = mock_routed_command(&bin, tmp.path(), mock)
        .args(["ask", "authentication", "--format", "json"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "a mid-build disconnect should degrade, not fail: {stderr}"
    );
    assert!(
        stderr.contains("falling back to the bundled provider"),
        "expected fallback warning on stderr, got: {stderr}"
    );
    assert!(
        output.stdout.is_empty(),
        "no structured results are possible on this path, got: {}",
        String::from_utf8_lossy(&output.stdout)
    );
}

/// Mid-session disconnect at the query embed: the stored vectors already
/// belong to the ollama space (padded to valid 768-float blobs so they
/// load), so the probe succeeding keeps the provider ollama until the query
/// embed dies. The mid-embed fallback must then refuse — degrading to
/// bundled would silently drop the indexed corpus — surfacing the mismatch
/// with the exact re-embed command.
#[test]
fn ask_mid_embed_disconnect_over_foreign_space_blocks_with_reembed_command() {
    let bin = wonk_bin();
    assert!(bin.exists(), "wonk binary not found at {}", bin.display());
    let tmp = indexed_bundled_repo(&bin);

    // Relabel into the ollama space and pad the vectors to real 768-float
    // blobs so they load as valid ollama vectors (a plain relabel keeps
    // 256-float blobs, which would fail vector decoding before the query
    // embed ever runs).
    let mut vector = Vec::with_capacity(768 * 4);
    for _ in 0..768 {
        vector.extend_from_slice(&0.25_f32.to_le_bytes());
    }
    let conn = rusqlite::Connection::open(tmp.path().join(".wonk/index.db")).unwrap();
    let changed = conn
        .execute(
            "UPDATE embeddings SET provider = 'ollama', dim = 768, vector = ?1",
            rusqlite::params![vector],
        )
        .unwrap();
    assert!(changed > 0, "expected existing embeddings to relabel");
    drop(conn);

    fs::create_dir_all(tmp.path().join(".wonk")).unwrap();
    fs::write(
        tmp.path().join(".wonk/config.toml"),
        "[embedding]\nprovider = \"ollama\"\n",
    )
    .unwrap();

    let mock = spawn_mock_ollama_stopped_after_the_probe();
    let output = mock_routed_command(&bin, tmp.path(), mock)
        .args(["ask", "authentication", "--format", "json"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "the mid-embed fallback must refuse to search a foreign space: {stderr}"
    );
    assert!(
        stderr.contains("vector space mismatch"),
        "expected the re-plan's mismatch error (not a plain transport failure), got: {stderr}"
    );
    assert!(
        stderr.contains("active bundled/256"),
        "expected the bundled fallback space in the error, got: {stderr}"
    );
    assert!(
        stderr.contains("stored ollama/768"),
        "expected the stored ollama space in the error, got: {stderr}"
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
