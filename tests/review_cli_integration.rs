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

// -- Filter, cap, and suppression lifecycle (TASK-089) ------------------------

/// A repo with three independent multi-line functions (so a body edit
/// never touches the signature line) — one diff can carry a breaking
/// change (delete used) plus two coverage gaps (edit the bodies).
fn indexed_repo_three_fns() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    git(root, &["init", "-b", "main"]);
    git(root, &["config", "user.email", "test@test.com"]);
    git(root, &["config", "user.name", "Test"]);

    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/lib.rs"), THREE_FNS_BASE).unwrap();
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

const THREE_FNS_BASE: &str = "pub fn used() -> i32 {\n    1\n}\n\npub fn caller() -> i32 {\n    used()\n}\n\npub fn other() -> i32 {\n    7\n}\n";

const THREE_FNS_USED_EDITED: &str = "pub fn used() -> i32 {\n    2\n}\n\npub fn caller() -> i32 {\n    used()\n}\n\npub fn other() -> i32 {\n    7\n}\n";

const THREE_FNS_BOTH_EDITED: &str = "pub fn used() -> i32 {\n    2\n}\n\npub fn caller() -> i32 {\n    used()\n}\n\npub fn other() -> i32 {\n    8\n}\n";

const THREE_FNS_USED_DELETED: &str =
    "pub fn caller() -> i32 {\n    used()\n}\n\npub fn other() -> i32 {\n    8\n}\n";

/// Run a `review suppress` subcommand without --format/--quiet so the
/// confirmation hints land on stderr (structured formats suppress them).
fn run_suppress(repo: &std::path::Path, args: &[&str]) -> (i32, String, String) {
    let mut cmd = Command::new(wonk_bin());
    cmd.args(args).current_dir(repo);
    let out = cmd.output().unwrap();
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn review_max_findings_caps_worst_first_and_counts_over_cap() {
    let repo = indexed_repo_three_fns();
    let root = repo.path();

    // BLOCK (removed used with a live caller) + WARNING (other's body edit).
    std::fs::write(root.join("src/lib.rs"), THREE_FNS_USED_DELETED).unwrap();

    let (code, stdout, stderr) = run_review(root, &["--max-findings", "1"]);
    assert_eq!(code, 0, "{stderr}");
    let (findings, verdict) = parse_review_ndjson(&stdout);
    assert_eq!(findings.len(), 1, "cap trims to one line: {stdout}");
    assert_eq!(findings[0]["kind"], "breaking-change", "the worst survives");
    assert_eq!(verdict["finding_count"], 1);
    assert_eq!(verdict["drops"]["over_cap"], 1);
    assert_eq!(verdict["drops"]["below_confidence"], 0);
}

#[test]
fn review_verdict_line_always_carries_five_zeroed_drop_keys() {
    let repo = indexed_repo();
    let root = repo.path();

    // No working-tree changes: a clean APPROVE still reports drops=0.
    let (code, stdout, stderr) = run_review(root, &[]);
    assert_eq!(code, 0, "{stderr}");
    let (_findings, verdict) = parse_review_ndjson(&stdout);
    let drops = &verdict["drops"];
    assert!(drops.is_object(), "always-present object: {verdict}");
    for key in [
        "below_confidence",
        "below_severity",
        "out_of_category",
        "over_cap",
        "identity_suppressed",
    ] {
        assert_eq!(drops[key], 0, "zero counts serialize: {verdict}");
    }
}

#[test]
fn review_suppress_lifecycle_retires_and_restores_a_finding() {
    let repo = indexed_repo_three_fns();
    let root = repo.path();

    // Body edit of used(): one coverage-gap warning (the signature line is
    // untouched, so rule A stays quiet).
    std::fs::write(root.join("src/lib.rs"), THREE_FNS_USED_EDITED).unwrap();

    let (code, stdout, stderr) = run_review(root, &[]);
    assert_eq!(code, 0, "{stderr}");
    let (findings, verdict) = parse_review_ndjson(&stdout);
    assert_eq!(findings.len(), 1);
    assert_eq!(verdict["verdict"], "REVIEW");
    let identity = findings[0]["identity"].as_str().unwrap().to_string();
    let rule = findings[0]["rule"].as_str().unwrap().to_string();

    // Suppress it (no --quiet: the confirmation lands on stderr).
    let (code, _stdout, stderr) = run_suppress(
        root,
        &[
            "review",
            "suppress",
            "add",
            &identity,
            "--rule",
            &rule,
            "--file",
            "src/lib.rs",
            "--note",
            "test-only helper",
        ],
    );
    assert_eq!(code, 0, "{stderr}");
    assert!(stderr.contains(&identity), "confirmed: {stderr}");

    // Re-review: gone, counted as identity_suppressed, verdict flips.
    let (code, stdout, stderr) = run_review(root, &[]);
    assert_eq!(code, 0, "{stderr}");
    let (findings, verdict) = parse_review_ndjson(&stdout);
    assert!(findings.is_empty(), "suppressed: {stdout}");
    assert_eq!(verdict["verdict"], "APPROVE");
    assert_eq!(verdict["drops"]["identity_suppressed"], 1);
    assert_eq!(verdict["drops"]["over_cap"], 0);

    // List shows the row with its retained metadata.
    let (code, stdout, stderr) =
        run_suppress(root, &["--format", "json", "review", "suppress", "list"]);
    assert_eq!(code, 0, "{stderr}");
    let rows: Vec<Value> = stdout
        .trim_end()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(rows.len(), 1, "{stdout}");
    assert_eq!(rows[0]["identity"], identity);
    assert_eq!(rows[0]["rule"], rule);
    assert_eq!(rows[0]["file"], "src/lib.rs");
    assert_eq!(rows[0]["note"], "test-only helper");

    // Remove it: the finding returns on the next review.
    let (code, _stdout, stderr) = run_suppress(root, &["review", "suppress", "remove", &identity]);
    assert_eq!(code, 0, "{stderr}");
    assert!(stderr.contains("removed 1 suppression(s)"), "{stderr}");

    let (code, stdout, stderr) = run_review(root, &[]);
    assert_eq!(code, 0, "{stderr}");
    let (findings, verdict) = parse_review_ndjson(&stdout);
    assert_eq!(findings.len(), 1, "restored after removal: {stdout}");
    assert_eq!(verdict["drops"]["identity_suppressed"], 0);
}

#[test]
fn review_suppress_bulk_remove_by_rule_removes_only_matching() {
    let repo = indexed_repo_three_fns();
    let root = repo.path();

    // Two coverage gaps: edit the bodies of used() and other().
    std::fs::write(root.join("src/lib.rs"), THREE_FNS_BOTH_EDITED).unwrap();

    let (code, stdout, stderr) = run_review(root, &[]);
    assert_eq!(code, 0, "{stderr}");
    let (findings, _verdict) = parse_review_ndjson(&stdout);
    let gaps: Vec<&Value> = findings
        .iter()
        .filter(|f| f["kind"] == "coverage-gap")
        .collect();
    assert_eq!(gaps.len(), 2, "two suppressible findings: {stdout}");

    // Two under rule A, one unrelated row under rule B.
    for f in &gaps {
        let id = f["identity"].as_str().unwrap();
        let (code, _o, stderr) =
            run_suppress(root, &["review", "suppress", "add", id, "--rule", "rule/a"]);
        assert_eq!(code, 0, "{stderr}");
    }
    let (code, _o, stderr) = run_suppress(
        root,
        &[
            "review",
            "suppress",
            "add",
            "zz-unrelated",
            "--rule",
            "rule/b",
        ],
    );
    assert_eq!(code, 0, "{stderr}");

    let (code, _o, stderr) =
        run_suppress(root, &["review", "suppress", "remove", "--rule", "rule/a"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(
        stderr.contains("removed 2 suppression(s)"),
        "exactly the matching two: {stderr}"
    );

    let (code, stdout, stderr) =
        run_suppress(root, &["--format", "json", "review", "suppress", "list"]);
    assert_eq!(code, 0, "{stderr}");
    let rows: Vec<Value> = stdout
        .trim_end()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(rows.len(), 1, "only rule/b survives: {stdout}");
    assert_eq!(rows[0]["rule"], "rule/b");
}

#[test]
fn review_min_severity_blocking_drops_warnings_only() {
    let repo = indexed_repo_three_fns();
    let root = repo.path();

    // Body edit of used() only: warnings only (no breaking change).
    std::fs::write(root.join("src/lib.rs"), THREE_FNS_USED_EDITED).unwrap();

    let (code, stdout, stderr) = run_review(root, &["--min-severity", "blocking"]);
    assert_eq!(code, 0, "{stderr}");
    let (findings, verdict) = parse_review_ndjson(&stdout);
    assert!(findings.is_empty(), "warnings dropped: {stdout}");
    assert_eq!(verdict["finding_count"], 0);
    assert_eq!(verdict["drops"]["below_severity"], 1);
    assert_eq!(verdict["drops"]["over_cap"], 0);
}

#[test]
fn review_kind_filter_drops_other_categories() {
    let repo = indexed_repo_three_fns();
    let root = repo.path();

    // One breaking change + one coverage gap; keep neither's category.
    std::fs::write(root.join("src/lib.rs"), THREE_FNS_USED_DELETED).unwrap();

    let (code, stdout, stderr) = run_review(root, &["--kind", "cross-repo"]);
    assert_eq!(code, 0, "{stderr}");
    let (findings, verdict) = parse_review_ndjson(&stdout);
    assert!(findings.is_empty(), "out-of-category dropped: {stdout}");
    assert_eq!(verdict["drops"]["out_of_category"], 2);
}
