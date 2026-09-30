//! Integration tests for the `wonk review` CLI (TASK-085).
//!
//! Spawns the built binary against a real git repo: JSON smoke with findings
//! and verdict and exit code 0 (the verdict is data), the no-index error path
//! (review never auto-initializes an index — indexing the current tree
//! mid-diff would fake an empty diff and a fake APPROVE), and the `--since`
//! compare sugar end to end.

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
    let v: Value = serde_json::from_str(stdout.trim_end()).unwrap();
    assert_eq!(v["scope"], "unstaged");
    assert_eq!(v["verdict"], "BLOCK");
    assert_eq!(v["findings"].as_array().map(Vec::len), Some(1));
    assert_eq!(v["findings"][0]["kind"], "breaking-change");
    assert_eq!(v["findings"][0]["anchor_method"], "old-side-line");
    assert_eq!(v["findings"][0]["line"], 1);
    assert_eq!(v["findings"][0]["related"][0]["name"], "caller");
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
    let v: Value = serde_json::from_str(stdout.trim_end()).unwrap();
    assert_eq!(v["scope"], "compare(main)");
    assert_eq!(v["verdict"], "BLOCK");
    assert_eq!(v["findings"][0]["kind"], "breaking-change");
}
