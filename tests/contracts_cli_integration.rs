//! Integration tests for the `wonk contracts` CLI (TASK-083).
//!
//! Spawns the built binary against a fixture repo and verifies the
//! acceptance criteria end to end: filtered listing with role/file/line/
//! confidence, within-repo orphans with no sibling repos, NDJSON output,
//! and exit code 2 (usage) for unknown filter values.

use std::path::PathBuf;
use std::process::{Command, Stdio};

use serde_json::Value;

/// Build the binary path. In test mode, cargo puts it in target/debug/.
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

/// A single-repo fixture: express routes in a named handler, an env read,
/// and an env write.
fn fixture_repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    Command::new("git")
        .args(["init"])
        .current_dir(root)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("src/app.js"),
        "const app = express();\n\nfunction setupRoutes() {\n  app.get('/v1/users/:id', getUser);\n}\n\nasync function load() {\n  const db = process.env.DATABASE_URL;\n  process.env.FEATURE_X = '1';\n}\n",
    )
    .unwrap();
    dir
}

fn run_contracts(repo: &std::path::Path, extra: &[&str]) -> (i32, String, String) {
    let mut cmd = Command::new(wonk_bin());
    cmd.arg("--quiet")
        .arg("contracts")
        .args(extra)
        .current_dir(repo);
    let out = cmd.output().unwrap();
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn contracts_kind_http_lists_routes() {
    let repo = fixture_repo();
    let (code, stdout, _stderr) = run_contracts(repo.path(), &["--kind", "http"]);
    assert_eq!(code, 0);
    assert_eq!(
        stdout.trim(),
        "src/app.js:4:http::GET::/v1/users/{p1} role=provider symbol=setupRoutes confidence=1.0"
    );
}

#[test]
fn contracts_orphans_single_repo_no_error() {
    let repo = fixture_repo();
    let (code, stdout, _stderr) = run_contracts(repo.path(), &["--orphans"]);
    assert_eq!(
        code, 0,
        "no sibling repos indexed: must succeed, got {stdout}"
    );
    assert_eq!(
        stdout.trim(),
        "src/app.js:8:env::::DATABASE_URL role=consumer symbol=load confidence=1.0"
    );
}

#[test]
fn contracts_ndjson_rows_parse() {
    let repo = fixture_repo();
    let (code, stdout, _stderr) = run_contracts(repo.path(), &["--format", "json"]);
    assert_eq!(code, 0);
    // Piped output follows the global single-line-per-file convention every
    // command shares (rows from one file joined with " ; "); each segment
    // is a complete JSON object, so consumption needs no post-processing
    // beyond line splitting.
    let segments: Vec<&str> = stdout.trim_end().split(" ; ").collect();
    assert_eq!(segments.len(), 3, "one object per contract: {stdout}");
    let parsed: Vec<Value> = segments
        .iter()
        .map(|s| serde_json::from_str(s).expect("each row must parse as JSON"))
        .collect();
    let ids: Vec<&str> = parsed
        .iter()
        .map(|v| v["canonical_id"].as_str().unwrap())
        .collect();
    assert_eq!(
        ids,
        vec![
            "env::::DATABASE_URL",
            "env::::FEATURE_X",
            "http::GET::/v1/users/{p1}"
        ]
    );
}

#[test]
fn contracts_unknown_kind_is_usage_error() {
    let repo = fixture_repo();
    let (code, _stdout, stderr) = run_contracts(repo.path(), &["--kind", "rest"]);
    assert_eq!(code, 2, "stderr: {stderr}");
    assert!(stderr.contains("unknown contract kind"), "stderr: {stderr}");
}
