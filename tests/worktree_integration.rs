//! Integration tests for git worktree support (PRD-WKT-REQ-001 through 005).
//!
//! These tests use real `git worktree` commands (not mocks) to verify that:
//! - Repo root detection accepts `.git` files (linked worktrees)
//! - Nearest root wins when worktrees are nested
//! - Indexing does not cross worktree boundaries
//! - The file watcher ignores events from nested worktrees
//! - Each worktree gets an independent index

use std::path::{Path, PathBuf};
use std::process::Command;

use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Test fixtures
// ---------------------------------------------------------------------------

/// Run a git command in the given directory, panicking on failure.
fn run_git(dir: &Path, args: &[&str]) {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .expect("git command failed to execute");
    assert!(
        output.status.success(),
        "git {:?} failed in {}: {}",
        args,
        dir.display(),
        String::from_utf8_lossy(&output.stderr),
    );
}

/// A sibling-worktree fixture: parent repo and a linked worktree in a
/// sibling directory (the common real-world pattern).
struct SiblingFixture {
    _dir: TempDir,
    parent_root: PathBuf,
    worktree_root: PathBuf,
}

impl SiblingFixture {
    fn new() -> Self {
        let dir = TempDir::new().unwrap();

        let parent_root = dir.path().join("parent");
        std::fs::create_dir(&parent_root).unwrap();
        run_git(&parent_root, &["init"]);
        run_git(&parent_root, &["config", "user.email", "test@test.com"]);
        run_git(&parent_root, &["config", "user.name", "Test"]);

        // Add source files unique to the parent and commit.
        std::fs::create_dir_all(parent_root.join("src")).unwrap();
        std::fs::write(
            parent_root.join("src/main.rs"),
            "fn parent_only() {}\nfn shared_func() {}",
        )
        .unwrap();
        run_git(&parent_root, &["add", "."]);
        run_git(&parent_root, &["commit", "-m", "initial"]);

        // Create a linked worktree as a sibling directory.
        let worktree_root = dir.path().join("worktree-branch");
        run_git(
            &parent_root,
            &[
                "worktree",
                "add",
                worktree_root.to_str().unwrap(),
                "-b",
                "wt-branch",
            ],
        );

        // Add a file unique to the worktree (not committed).
        std::fs::write(worktree_root.join("src/extra.rs"), "fn worktree_only() {}").unwrap();

        SiblingFixture {
            _dir: dir,
            parent_root,
            worktree_root,
        }
    }
}

/// A nested-worktree fixture: parent repo with a worktree placed inside
/// the parent's directory tree.
struct NestedFixture {
    _dir: TempDir,
    parent_root: PathBuf,
    nested_wt: PathBuf,
}

impl NestedFixture {
    fn new() -> Self {
        let dir = TempDir::new().unwrap();

        let parent_root = dir.path().join("repo");
        std::fs::create_dir(&parent_root).unwrap();
        run_git(&parent_root, &["init"]);
        run_git(&parent_root, &["config", "user.email", "test@test.com"]);
        run_git(&parent_root, &["config", "user.name", "Test"]);

        std::fs::create_dir_all(parent_root.join("src")).unwrap();
        std::fs::write(
            parent_root.join("src/main.rs"),
            "fn parent_func() {}\nfn shared_func() {}",
        )
        .unwrap();
        run_git(&parent_root, &["add", "."]);
        run_git(&parent_root, &["commit", "-m", "initial"]);

        // Create worktree INSIDE the parent's directory tree.
        let nested_wt = parent_root.join("worktrees/feature");
        run_git(
            &parent_root,
            &[
                "worktree",
                "add",
                nested_wt.to_str().unwrap(),
                "-b",
                "feature",
            ],
        );

        // Add a file unique to the nested worktree.
        std::fs::write(nested_wt.join("src/feature.rs"), "fn feature_func() {}").unwrap();

        NestedFixture {
            _dir: dir,
            parent_root,
            nested_wt,
        }
    }
}

// ---------------------------------------------------------------------------
// REQ-001: find_repo_root accepts .git files (linked worktrees)
// ---------------------------------------------------------------------------

#[test]
fn req_001_find_repo_root_accepts_git_file() {
    let fix = SiblingFixture::new();

    // The worktree root has a .git *file* (not directory).
    let git_entry = fix.worktree_root.join(".git");
    assert!(git_entry.exists(), ".git entry should exist in worktree");
    assert!(
        git_entry.is_file(),
        ".git should be a file in linked worktree, not a directory"
    );

    // find_repo_root should identify the worktree root.
    let found = wonk::db::find_repo_root(&fix.worktree_root.join("src")).unwrap();
    let expected = std::fs::canonicalize(&fix.worktree_root).unwrap();
    assert_eq!(found, expected);
}

// ---------------------------------------------------------------------------
// REQ-002: Nearest worktree root wins when nested
// ---------------------------------------------------------------------------

#[test]
fn req_002_nested_worktree_nearest_root_wins() {
    let fix = NestedFixture::new();

    // From inside the nested worktree's src/, find_repo_root should
    // return the nested worktree root, not the parent.
    let found = wonk::db::find_repo_root(&fix.nested_wt.join("src")).unwrap();
    let expected = std::fs::canonicalize(&fix.nested_wt).unwrap();
    let parent_canon = std::fs::canonicalize(&fix.parent_root).unwrap();

    assert_eq!(
        found, expected,
        "should find nested worktree root, not parent"
    );
    assert_ne!(found, parent_canon, "must NOT return the parent repo root");
}

// ---------------------------------------------------------------------------
// REQ-003: Parent index excludes nested worktree files
// ---------------------------------------------------------------------------

#[test]
fn req_003_parent_index_excludes_nested_worktree_files() {
    let fix = NestedFixture::new();

    // Verify the walker doesn't traverse into the nested worktree.
    let walked = wonk::walker::Walker::new(&fix.parent_root).collect_paths();
    let rel_paths: Vec<String> = walked
        .iter()
        .filter_map(|p| p.strip_prefix(&fix.parent_root).ok())
        .map(|p| p.to_string_lossy().into_owned())
        .collect();

    assert!(
        !rel_paths.iter().any(|p| p.contains("feature.rs")),
        "walker must not traverse into nested worktree, got: {rel_paths:?}"
    );
    assert!(
        rel_paths.iter().any(|p| p.contains("main.rs")),
        "walker should find parent's own files, got: {rel_paths:?}"
    );

    // Build index from the parent repo root (local mode).
    wonk::pipeline::build_index(&fix.parent_root, true).unwrap();

    // Open the parent's index and check file paths.
    let index_path = wonk::db::local_index_path(&fix.parent_root);
    let conn = wonk::db::open_existing(&index_path).unwrap();

    let files: Vec<String> = conn
        .prepare("SELECT path FROM files")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .filter_map(|r| r.ok())
        .collect();

    // Parent's own file should be present.
    assert!(
        files.iter().any(|f| f.contains("main.rs")),
        "parent index should contain src/main.rs, got: {files:?}"
    );

    // Nested worktree file should NOT be present.
    assert!(
        !files.iter().any(|f| f.contains("feature.rs")),
        "parent index must NOT contain nested worktree file feature.rs, got: {files:?}"
    );
}

// ---------------------------------------------------------------------------
// REQ-004: Parent watcher ignores nested worktree events
// ---------------------------------------------------------------------------

#[test]
fn req_004_parent_watcher_ignores_nested_worktree_events() {
    let fix = NestedFixture::new();

    // A file inside the nested worktree, expressed as a path relative
    // to the parent root.
    let nested_file_abs = fix.nested_wt.join("src/feature.rs");
    let nested_file_rel = nested_file_abs
        .strip_prefix(&fix.parent_root)
        .unwrap()
        .to_path_buf();

    // should_process must reject it.
    assert!(
        !wonk::watcher::should_process(&nested_file_rel, &fix.parent_root),
        "watcher should reject events from nested worktree: {nested_file_rel:?}"
    );

    // But the parent's own files should pass.
    assert!(
        wonk::watcher::should_process(Path::new("src/main.rs"), &fix.parent_root),
        "watcher should accept events from parent's own files"
    );
}

// ---------------------------------------------------------------------------
// REQ-005: Separate indexes per worktree
// ---------------------------------------------------------------------------

#[test]
fn req_005_separate_indexes_per_worktree() {
    let fix = SiblingFixture::new();

    // Build indexes for both (local mode).
    let parent_stats = wonk::pipeline::build_index(&fix.parent_root, true).unwrap();
    let wt_stats = wonk::pipeline::build_index(&fix.worktree_root, true).unwrap();

    assert!(
        parent_stats.file_count > 0,
        "parent should have indexed files"
    );
    assert!(
        wt_stats.file_count > 0,
        "worktree should have indexed files"
    );

    // Index paths must be different.
    let parent_idx = wonk::db::local_index_path(&fix.parent_root);
    let wt_idx = wonk::db::local_index_path(&fix.worktree_root);
    assert_ne!(
        parent_idx, wt_idx,
        "index paths must differ between worktrees"
    );

    // Central repo hashes would also differ.
    let parent_hash = wonk::db::repo_hash(&std::fs::canonicalize(&fix.parent_root).unwrap());
    let wt_hash = wonk::db::repo_hash(&std::fs::canonicalize(&fix.worktree_root).unwrap());
    assert_ne!(
        parent_hash, wt_hash,
        "repo hashes must differ for different worktree roots"
    );

    // Open both indexes and verify content differs.
    let parent_conn = wonk::db::open_existing(&parent_idx).unwrap();
    let wt_conn = wonk::db::open_existing(&wt_idx).unwrap();

    let parent_symbols: Vec<String> = parent_conn
        .prepare("SELECT DISTINCT name FROM symbols")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .filter_map(|r| r.ok())
        .collect();

    let wt_symbols: Vec<String> = wt_conn
        .prepare("SELECT DISTINCT name FROM symbols")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .filter_map(|r| r.ok())
        .collect();

    // The worktree has `worktree_only` which the parent does not.
    assert!(
        wt_symbols.iter().any(|s| s == "worktree_only"),
        "worktree index should contain 'worktree_only', got: {wt_symbols:?}"
    );
    assert!(
        !parent_symbols.iter().any(|s| s == "worktree_only"),
        "parent index should NOT contain 'worktree_only', got: {parent_symbols:?}"
    );

    // Both share `shared_func` (from the initial commit).
    assert!(
        parent_symbols.iter().any(|s| s == "shared_func"),
        "parent should have 'shared_func', got: {parent_symbols:?}"
    );
    assert!(
        wt_symbols.iter().any(|s| s == "shared_func"),
        "worktree should have 'shared_func', got: {wt_symbols:?}"
    );
}
