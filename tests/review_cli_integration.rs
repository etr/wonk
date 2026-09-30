//! Integration tests for the `wonk review` CLI (TASK-085, widened 086).
//!
//! Spawns the built binary against real git repos: NDJSON smoke (one line
//! per finding, one final verdict line — PRD-REV-REQ-009) with exit code 0
//! (the verdict is data), the no-index error path (review never
//! auto-initializes an index — indexing the current tree mid-diff would
//! fake an empty diff and a fake APPROVE), the `--since` compare sugar end
//! to end, and the cross-repo rule end to end against an isolated
//! `$HOME/.wonk/repos` registry.

use std::path::PathBuf;
use std::process::{Command, Stdio};

use serde_json::Value;

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

fn git(repo: &std::path::Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(repo)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert!(status.success(), "git {:?} failed", args);
}

/// Parse NDJSON review output: finding lines plus the one verdict line
/// (discriminated by key presence — the verdict line is the only one
/// carrying `verdict`).
fn parse_review_ndjson(stdout: &str) -> (Vec<Value>, Value) {
    let lines: Vec<&str> = stdout.trim_end().lines().collect();
    assert!(!lines.is_empty(), "review always emits a verdict line");
    let mut findings = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        let v: Value = serde_json::from_str(line)
            .unwrap_or_else(|e| panic!("line {i} is not independent JSON ({e}): {line}"));
        if v.get("verdict").is_some() {
            assert_eq!(
                i,
                lines.len() - 1,
                "verdict line must be last, got it at {i}: {stdout}"
            );
            return (findings, v);
        }
        findings.push(v);
    }
    panic!("no verdict line in: {stdout}");
}

/// Real git repo on branch main with an initial commit of a used()+caller()
/// pair and a wonk index reflecting that commit (the base state).
fn indexed_repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    git(root, &["init", "-b", "main"]);
    git(root, &["config", "user.email", "test@test.com"]);
    git(root, &["config", "user.name", "Test"]);

    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("src/lib.rs"),
        "pub fn used() {}\n\npub fn caller() { used(); }\n",
    )
    .unwrap();
    git(root, &["add", "."]);
    git(root, &["commit", "-m", "initial"]);

    let out = Command::new(wonk_bin())
        .arg("--quiet")
        .arg("init")
        .current_dir(root)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "wonk init failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    dir
}

fn run_review(repo: &std::path::Path, extra: &[&str]) -> (i32, String, String) {
    let mut cmd = Command::new(wonk_bin());
    cmd.arg("--quiet")
        .arg("--format")
        .arg("json")
        .arg("review")
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
fn review_json_smoke_reports_findings_and_verdict_with_exit_zero() {
    let repo = indexed_repo();
    let root = repo.path();

    // Working tree deletes used(), keeps caller.
    std::fs::write(root.join("src/lib.rs"), "pub fn caller() { used(); }\n").unwrap();

    let (code, stdout, stderr) = run_review(root, &[]);
    assert_eq!(code, 0, "verdict is data, exit code stays 0: {stderr}");
    let (findings, verdict) = parse_review_ndjson(&stdout);
    assert_eq!(verdict["scope"], "unstaged");
    assert_eq!(verdict["verdict"], "BLOCK");
    assert_eq!(verdict["finding_count"], 1);
    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0]["kind"], "breaking-change");
    assert_eq!(findings[0]["anchor_method"], "old-side-line");
    assert_eq!(findings[0]["line"], 1);
    assert_eq!(findings[0]["related"][0]["name"], "caller");
}

#[test]
fn review_without_index_errors_instead_of_auto_indexing() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    git(root, &["init", "-b", "main"]);
    git(root, &["config", "user.email", "test@test.com"]);
    git(root, &["config", "user.name", "Test"]);
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("src/lib.rs"),
        "pub fn used() {}\n\npub fn caller() { used(); }\n",
    )
    .unwrap();
    git(root, &["add", "."]);
    git(root, &["commit", "-m", "initial"]);

    // No `wonk init`: review must fail loudly, never index the current tree
    // (which would fake an empty diff and a fake APPROVE).
    let (code, _stdout, stderr) = run_review(root, &[]);
    assert_ne!(code, 0, "missing index must be an error");
    assert!(
        stderr.contains("no index found"),
        "error must say what is missing: {stderr}"
    );
    assert!(
        stderr.contains("base") || stderr.contains("wonk init"),
        "error must hint how to fix it: {stderr}"
    );
}

#[test]
fn review_since_ref_reviews_committed_feature_work() {
    let repo = indexed_repo();
    let root = repo.path();

    // Feature work leaves main: the index (built at main) stays the base
    // state, and --since main diffs the working tree against it.
    git(root, &["checkout", "-b", "feature"]);
    std::fs::write(root.join("src/lib.rs"), "pub fn caller() { used(); }\n").unwrap();
    git(root, &["add", "."]);
    git(root, &["commit", "-m", "remove used"]);

    let (code, stdout, stderr) = run_review(root, &["--since", "main"]);
    assert_eq!(code, 0, "{stderr}");
    let (findings, verdict) = parse_review_ndjson(&stdout);
    assert_eq!(verdict["scope"], "compare(main)");
    assert_eq!(verdict["verdict"], "BLOCK");
    assert_eq!(findings[0]["kind"], "breaking-change");
}

// -- cross-repo rule end to end (TASK-086, PRD-REV-REQ-010) -------------------

const CROSS_REPO_ROUTES: &str = "const app = express();\nfunction registerUserRoutes() {\n  app.get('/v1/users', getUser);\n}\n";
const CROSS_REPO_ROUTES_EDITED: &str = "const app = express();\nfunction registerUserRoutes() {\n  app.get('/v1/users', getUserV2);\n}\n";
const SIBLING_CLIENT: &str =
    "async function loadUsers() {\n  await fetch('https://api.io/v1/users');\n}\n";

/// Isolated `$HOME` holding the central registry both repos index into.
fn registry_home() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

/// Create a git repo with the given files and workspace declaration, then
/// index it into `home`'s registry (`wonk init` under the isolated HOME).
fn indexed_workspace_repo(
    home: &std::path::Path,
    name: &str,
    workspace: &str,
    files: &[(&str, &str)],
) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join(name);
    std::fs::create_dir_all(&root).unwrap();
    git(&root, &["init", "-b", "main"]);
    git(&root, &["config", "user.email", "test@test.com"]);
    git(&root, &["config", "user.name", "Test"]);
    for (path, content) in files {
        if let Some(parent) = std::path::Path::new(path).parent() {
            std::fs::create_dir_all(root.join(parent)).unwrap();
        }
        std::fs::write(root.join(path), content).unwrap();
    }
    std::fs::create_dir_all(root.join(".wonk")).unwrap();
    std::fs::write(
        root.join(".wonk/config.toml"),
        format!("[contracts]\nworkspace = \"{workspace}\"\n"),
    )
    .unwrap();
    git(&root, &["add", "."]);
    git(&root, &["commit", "-m", "initial"]);

    let out = Command::new(wonk_bin())
        .env("HOME", home)
        .arg("--quiet")
        .arg("init")
        .current_dir(&root)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "wonk init failed for {name}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    dir
}

#[test]
fn cross_repo_review_names_consuming_repo_end_to_end() {
    let home = registry_home();
    let provider = indexed_workspace_repo(
        home.path(),
        "users-svc",
        "payments",
        &[("src/routes.js", CROSS_REPO_ROUTES)],
    );
    let _sibling = indexed_workspace_repo(
        home.path(),
        "own-api",
        "payments",
        &[("src/client.js", SIBLING_CLIENT)],
    );
    let root = provider.path().join("users-svc");

    // Body edit of the route registrar: the signature is untouched, but a
    // sibling repo consumes the provided contract.
    std::fs::write(root.join("src/routes.js"), CROSS_REPO_ROUTES_EDITED).unwrap();

    let out = Command::new(wonk_bin())
        .env("HOME", home.path())
        .arg("--quiet")
        .arg("--format")
        .arg("json")
        .arg("review")
        .current_dir(&root)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let (findings, verdict) = parse_review_ndjson(&stdout);
    let cross: Vec<&Value> = findings
        .iter()
        .filter(|f| f["kind"] == "cross-repo")
        .collect();
    assert_eq!(cross.len(), 1, "got: {findings:?}");
    assert!(
        cross[0]["message"]
            .as_str()
            .is_some_and(|m| m.contains("own-api")),
        "consuming repo named: {cross:?}"
    );
    assert_eq!(cross[0]["related"][0]["file"], "own-api:src/client.js");
    assert_eq!(verdict["verdict"], "REVIEW");
}
