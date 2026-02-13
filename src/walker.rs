//! File walker with gitignore support, default exclusions, and worktree
//! boundary detection.
//!
//! Wraps the `ignore` crate's `WalkBuilder` to provide a file walker that:
//! - Respects `.gitignore` rules
//! - Respects `.wonkignore` files (same syntax as `.gitignore`)
//! - Applies additional ignore patterns from config (`[ignore].patterns`)
//! - Skips common build/dependency directories by default
//! - Skips hidden files/directories except `.github`
//! - Skips nested repositories and linked worktrees (directories containing
//!   a `.git` entry that are not the walk root) to prevent cross-worktree
//!   contamination during indexing
//! - Supports path restriction (walking from a subdirectory)
//! - Supports parallel file enumeration via `WalkParallel`

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use ignore::overrides::OverrideBuilder;
use ignore::{WalkBuilder, WalkState};

/// Directories that are always excluded from walks, regardless of `.gitignore`.
const DEFAULT_EXCLUSIONS: &[&str] = &[
    "node_modules",
    "vendor",
    "target",
    "build",
    "dist",
    "__pycache__",
    ".venv",
];

/// Hidden directory names that are NOT excluded (i.e., they are allowed
/// even though hidden directories are otherwise skipped).
const HIDDEN_ALLOWLIST: &[&str] = &[".github"];

/// A file-system walker that respects `.gitignore`, `.wonkignore`, and
/// applies default exclusions plus optional config-driven ignore patterns.
pub struct Walker {
    root: PathBuf,
    threads: usize,
    /// Additional ignore patterns (gitignore syntax) supplied via config.
    ignore_patterns: Vec<String>,
}

impl Walker {
    /// Create a new walker rooted at the given path.
    ///
    /// The path may be a subdirectory of a repository; the walker will still
    /// respect `.gitignore` files from parent directories.
    pub fn new<P: AsRef<Path>>(root: P) -> Self {
        Self {
            root: root.as_ref().to_path_buf(),
            threads: 0, // 0 means ignore crate picks a sensible default
            ignore_patterns: Vec::new(),
        }
    }

    /// Set the number of threads used for parallel walking.
    ///
    /// A value of `0` (the default) lets the `ignore` crate choose.
    pub fn threads(mut self, n: usize) -> Self {
        self.threads = n;
        self
    }

    /// Supply additional ignore patterns (gitignore syntax) from config.
    ///
    /// These patterns are applied as overrides and use the same syntax as
    /// `.gitignore`.  They are combined with the default exclusions and
    /// any `.wonkignore` rules.
    pub fn with_ignore_patterns(mut self, patterns: &[String]) -> Self {
        self.ignore_patterns = patterns.to_vec();
        self
    }

    /// Build the underlying `WalkBuilder` with all our configuration applied.
    fn make_builder(&self) -> WalkBuilder {
        let mut builder = WalkBuilder::new(&self.root);

        // Let the ignore crate handle .gitignore, .ignore, etc.
        builder.standard_filters(true);

        // Register `.wonkignore` as a custom ignore filename.  The ignore
        // crate will look for this file in every directory during the walk
        // and apply its patterns (same syntax as `.gitignore`).
        builder.add_custom_ignore_filename(".wonkignore");

        // We disable the built-in hidden filter because we need a more
        // nuanced policy (skip hidden except for allowlisted names).
        builder.hidden(false);

        // Build overrides that negate (exclude) the default directories
        // and any additional config-supplied patterns.
        let mut overrides = OverrideBuilder::new(&self.root);
        for dir in DEFAULT_EXCLUSIONS {
            // The `!` prefix in override globs means "exclude this pattern".
            let pattern = format!("!{dir}/");
            overrides
                .add(&pattern)
                .expect("default exclusion pattern should be valid");
        }

        // Add config-driven ignore patterns as exclusion overrides.
        for pattern in &self.ignore_patterns {
            let negated = format!("!{pattern}");
            overrides
                .add(&negated)
                .expect("config ignore pattern should be valid");
        }

        builder.overrides(
            overrides.build().expect("override builder should succeed"),
        );

        // Custom filter: skip hidden entries and worktree/nested-repo boundaries.
        builder.filter_entry(|entry| {
            let name = entry.file_name().to_string_lossy();

            // Skip hidden entries (name starts with `.`) unless allowlisted.
            if name.starts_with('.') {
                // The root entry itself (depth 0) always passes through.
                if entry.depth() == 0 {
                    return true;
                }
                return HIDDEN_ALLOWLIST.iter().any(|a| *a == &*name);
            }

            // Worktree boundary: skip non-root directories that contain a
            // `.git` entry (either a directory for nested repos, or a file
            // for linked worktrees).
            if entry.depth() > 0 {
                if let Some(ft) = entry.file_type() {
                    if ft.is_dir() && entry.path().join(".git").exists() {
                        return false;
                    }
                }
            }

            true
        });

        if self.threads > 0 {
            builder.threads(self.threads);
        }

        builder
    }

    /// Walk the file tree sequentially and collect all matching file paths.
    pub fn collect_paths(&self) -> Vec<PathBuf> {
        let builder = self.make_builder();
        let mut paths = Vec::new();
        for result in builder.build() {
            let entry = match result {
                Ok(e) => e,
                Err(_) => continue,
            };
            // Only collect files, not directories.
            if entry
                .file_type()
                .map_or(false, |ft| ft.is_file())
            {
                paths.push(entry.into_path());
            }
        }
        paths
    }

    /// Walk the file tree in parallel and collect all matching file paths.
    ///
    /// This uses the `ignore` crate's `WalkParallel` for concurrent directory
    /// traversal across multiple threads.
    pub fn collect_paths_parallel(&self) -> Vec<PathBuf> {
        let builder = self.make_builder();
        let paths: Arc<Mutex<Vec<PathBuf>>> = Arc::new(Mutex::new(Vec::new()));
        let parallel = builder.build_parallel();

        parallel.run(|| {
            let paths = Arc::clone(&paths);
            Box::new(move |result| {
                let entry = match result {
                    Ok(e) => e,
                    Err(_) => return WalkState::Continue,
                };
                if entry
                    .file_type()
                    .map_or(false, |ft| ft.is_file())
                {
                    paths.lock().unwrap().push(entry.into_path());
                }
                WalkState::Continue
            })
        });

        Arc::try_unwrap(paths)
            .expect("all threads should have finished")
            .into_inner()
            .unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Helper: create a temporary directory tree for testing.
    struct TestDir {
        dir: tempfile::TempDir,
    }

    impl TestDir {
        fn new() -> Self {
            Self {
                dir: tempfile::tempdir().unwrap(),
            }
        }

        fn path(&self) -> &Path {
            self.dir.path()
        }

        /// Create a file (and any necessary parent directories).
        fn create_file(&self, relative: &str) {
            let p = self.dir.path().join(relative);
            if let Some(parent) = p.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(&p, "content").unwrap();
        }
    }

    /// Collect paths relative to the test root, sorted for determinism.
    fn sorted_relative(root: &Path, paths: &[PathBuf]) -> Vec<String> {
        let mut rel: Vec<String> = paths
            .iter()
            .filter_map(|p| {
                p.strip_prefix(root)
                    .ok()
                    .map(|r| r.to_string_lossy().into_owned())
            })
            .collect();
        rel.sort();
        rel
    }

    #[test]
    fn respects_gitignore() {
        let td = TestDir::new();
        // The ignore crate only respects .gitignore inside a git repository,
        // so we need to `git init` the temp directory.
        fs::create_dir(td.path().join(".git")).unwrap();
        td.create_file("keep.rs");
        td.create_file("ignored.log");
        // Write a .gitignore that excludes *.log
        fs::write(td.path().join(".gitignore"), "*.log\n").unwrap();

        let walker = Walker::new(td.path());
        let paths = walker.collect_paths();
        let rel = sorted_relative(td.path(), &paths);

        assert!(rel.contains(&"keep.rs".to_string()));
        assert!(!rel.contains(&"ignored.log".to_string()));
    }

    #[test]
    fn skips_default_exclusions() {
        let td = TestDir::new();
        td.create_file("src/main.rs");
        td.create_file("node_modules/pkg/index.js");
        td.create_file("vendor/lib.go");
        td.create_file("target/debug/bin");
        td.create_file("build/output.js");
        td.create_file("dist/bundle.js");
        td.create_file("__pycache__/mod.pyc");
        td.create_file(".venv/bin/python");

        let walker = Walker::new(td.path());
        let paths = walker.collect_paths();
        let rel = sorted_relative(td.path(), &paths);

        assert!(rel.contains(&"src/main.rs".to_string()), "src/main.rs should be present, got: {rel:?}");
        assert!(!rel.iter().any(|p| p.starts_with("node_modules")), "node_modules should be excluded");
        assert!(!rel.iter().any(|p| p.starts_with("vendor")), "vendor should be excluded");
        assert!(!rel.iter().any(|p| p.starts_with("target")), "target should be excluded");
        assert!(!rel.iter().any(|p| p.starts_with("build")), "build should be excluded");
        assert!(!rel.iter().any(|p| p.starts_with("dist")), "dist should be excluded");
        assert!(!rel.iter().any(|p| p.starts_with("__pycache__")), "__pycache__ should be excluded");
        assert!(!rel.iter().any(|p| p.starts_with(".venv")), ".venv should be excluded");
    }

    #[test]
    fn skips_hidden_except_github() {
        let td = TestDir::new();
        td.create_file("visible.rs");
        td.create_file(".hidden/secret.txt");
        td.create_file(".github/workflows/ci.yml");
        td.create_file(".config/settings.toml");

        let walker = Walker::new(td.path());
        let paths = walker.collect_paths();
        let rel = sorted_relative(td.path(), &paths);

        assert!(rel.contains(&"visible.rs".to_string()));
        assert!(
            rel.iter().any(|p| p.starts_with(".github")),
            ".github should be allowed, got: {rel:?}"
        );
        assert!(
            !rel.iter().any(|p| p.starts_with(".hidden")),
            ".hidden should be skipped"
        );
        assert!(
            !rel.iter().any(|p| p.starts_with(".config")),
            ".config should be skipped"
        );
    }

    #[test]
    fn path_restriction_works() {
        let td = TestDir::new();
        td.create_file("src/main.rs");
        td.create_file("src/lib.rs");
        td.create_file("tests/integration.rs");

        // Walk only the src/ subdirectory.
        let walker = Walker::new(td.path().join("src"));
        let paths = walker.collect_paths();
        let rel: Vec<String> = paths
            .iter()
            .filter_map(|p| {
                p.strip_prefix(td.path().join("src"))
                    .ok()
                    .map(|r| r.to_string_lossy().into_owned())
            })
            .collect();

        assert!(rel.contains(&"main.rs".to_string()));
        assert!(rel.contains(&"lib.rs".to_string()));
        assert!(!rel.iter().any(|p| p.contains("integration")));
    }

    #[test]
    fn parallel_walk_finds_same_files() {
        let td = TestDir::new();
        td.create_file("a.rs");
        td.create_file("b.rs");
        td.create_file("sub/c.rs");
        td.create_file("node_modules/pkg/d.js");
        td.create_file(".hidden/e.txt");

        let walker = Walker::new(td.path());
        let mut seq = sorted_relative(td.path(), &walker.collect_paths());
        let mut par = sorted_relative(td.path(), &walker.collect_paths_parallel());
        seq.sort();
        par.sort();

        assert_eq!(seq, par, "sequential and parallel walks should find the same files");
    }

    // ----- .wonkignore tests -----

    #[test]
    fn wonkignore_excludes_matching_files() {
        let td = TestDir::new();
        td.create_file("keep.rs");
        td.create_file("notes.md");
        td.create_file("debug.log");
        td.create_file("sub/trace.log");

        // Write a .wonkignore that excludes *.log files.
        fs::write(td.path().join(".wonkignore"), "*.log\n").unwrap();

        let walker = Walker::new(td.path());
        let paths = walker.collect_paths();
        let rel = sorted_relative(td.path(), &paths);

        assert!(rel.contains(&"keep.rs".to_string()), "keep.rs should be present, got: {rel:?}");
        assert!(rel.contains(&"notes.md".to_string()), "notes.md should be present, got: {rel:?}");
        assert!(
            !rel.iter().any(|p| p.ends_with(".log")),
            ".log files should be excluded by .wonkignore, got: {rel:?}"
        );
    }

    #[test]
    fn wonkignore_excludes_directories() {
        let td = TestDir::new();
        td.create_file("src/main.rs");
        td.create_file("generated/output.rs");
        td.create_file("generated/deep/nested.rs");

        // Write a .wonkignore that excludes the generated/ directory.
        fs::write(td.path().join(".wonkignore"), "generated/\n").unwrap();

        let walker = Walker::new(td.path());
        let paths = walker.collect_paths();
        let rel = sorted_relative(td.path(), &paths);

        assert!(rel.contains(&"src/main.rs".to_string()), "src/main.rs should be present");
        assert!(
            !rel.iter().any(|p| p.starts_with("generated")),
            "generated/ should be excluded by .wonkignore, got: {rel:?}"
        );
    }

    #[test]
    fn wonkignore_in_subdirectory() {
        let td = TestDir::new();
        td.create_file("root.rs");
        td.create_file("sub/keep.rs");
        td.create_file("sub/skip.tmp");

        // Write a .wonkignore in the sub/ directory.
        fs::write(td.path().join("sub/.wonkignore"), "*.tmp\n").unwrap();

        let walker = Walker::new(td.path());
        let paths = walker.collect_paths();
        let rel = sorted_relative(td.path(), &paths);

        assert!(rel.contains(&"root.rs".to_string()));
        assert!(rel.contains(&"sub/keep.rs".to_string()));
        assert!(
            !rel.iter().any(|p| p.ends_with(".tmp")),
            ".tmp files should be excluded by sub/.wonkignore, got: {rel:?}"
        );
    }

    #[test]
    fn wonkignore_parallel_matches_sequential() {
        let td = TestDir::new();
        td.create_file("a.rs");
        td.create_file("b.log");
        td.create_file("sub/c.rs");
        td.create_file("sub/d.log");

        fs::write(td.path().join(".wonkignore"), "*.log\n").unwrap();

        let walker = Walker::new(td.path());
        let mut seq = sorted_relative(td.path(), &walker.collect_paths());
        let mut par = sorted_relative(td.path(), &walker.collect_paths_parallel());
        seq.sort();
        par.sort();

        assert_eq!(seq, par, "sequential and parallel walks should match with .wonkignore");
        assert!(!seq.iter().any(|p| p.ends_with(".log")));
    }

    // ----- Config ignore patterns tests -----

    #[test]
    fn config_ignore_patterns_exclude_files() {
        let td = TestDir::new();
        td.create_file("keep.rs");
        td.create_file("data.csv");
        td.create_file("sub/report.csv");
        td.create_file("notes.txt");

        let patterns = vec!["*.csv".to_string()];
        let walker = Walker::new(td.path()).with_ignore_patterns(&patterns);
        let paths = walker.collect_paths();
        let rel = sorted_relative(td.path(), &paths);

        assert!(rel.contains(&"keep.rs".to_string()));
        assert!(rel.contains(&"notes.txt".to_string()));
        assert!(
            !rel.iter().any(|p| p.ends_with(".csv")),
            ".csv files should be excluded by config patterns, got: {rel:?}"
        );
    }

    #[test]
    fn config_ignore_patterns_exclude_directories() {
        let td = TestDir::new();
        td.create_file("src/main.rs");
        td.create_file("tmp/scratch.txt");
        td.create_file("tmp/deep/file.txt");

        let patterns = vec!["tmp/".to_string()];
        let walker = Walker::new(td.path()).with_ignore_patterns(&patterns);
        let paths = walker.collect_paths();
        let rel = sorted_relative(td.path(), &paths);

        assert!(rel.contains(&"src/main.rs".to_string()));
        assert!(
            !rel.iter().any(|p| p.starts_with("tmp")),
            "tmp/ should be excluded by config patterns, got: {rel:?}"
        );
    }

    #[test]
    fn config_ignore_multiple_patterns() {
        let td = TestDir::new();
        td.create_file("keep.rs");
        td.create_file("data.csv");
        td.create_file("debug.log");
        td.create_file("backup.bak");

        let patterns = vec![
            "*.csv".to_string(),
            "*.log".to_string(),
            "*.bak".to_string(),
        ];
        let walker = Walker::new(td.path()).with_ignore_patterns(&patterns);
        let paths = walker.collect_paths();
        let rel = sorted_relative(td.path(), &paths);

        assert_eq!(rel, vec!["keep.rs".to_string()], "only keep.rs should remain, got: {rel:?}");
    }

    #[test]
    fn config_patterns_parallel_matches_sequential() {
        let td = TestDir::new();
        td.create_file("a.rs");
        td.create_file("b.csv");
        td.create_file("sub/c.rs");
        td.create_file("sub/d.csv");

        let patterns = vec!["*.csv".to_string()];
        let walker = Walker::new(td.path()).with_ignore_patterns(&patterns);
        let mut seq = sorted_relative(td.path(), &walker.collect_paths());
        let mut par = sorted_relative(td.path(), &walker.collect_paths_parallel());
        seq.sort();
        par.sort();

        assert_eq!(seq, par, "sequential and parallel should match with config patterns");
        assert!(!seq.iter().any(|p| p.ends_with(".csv")));
    }

    #[test]
    fn wonkignore_and_config_patterns_combine() {
        let td = TestDir::new();
        td.create_file("keep.rs");
        td.create_file("debug.log");
        td.create_file("data.csv");
        td.create_file("notes.txt");

        // .wonkignore excludes *.log
        fs::write(td.path().join(".wonkignore"), "*.log\n").unwrap();

        // Config patterns exclude *.csv
        let patterns = vec!["*.csv".to_string()];
        let walker = Walker::new(td.path()).with_ignore_patterns(&patterns);
        let paths = walker.collect_paths();
        let rel = sorted_relative(td.path(), &paths);

        assert!(rel.contains(&"keep.rs".to_string()));
        assert!(rel.contains(&"notes.txt".to_string()));
        assert!(
            !rel.iter().any(|p| p.ends_with(".log")),
            ".log should be excluded by .wonkignore, got: {rel:?}"
        );
        assert!(
            !rel.iter().any(|p| p.ends_with(".csv")),
            ".csv should be excluded by config patterns, got: {rel:?}"
        );
    }

    #[test]
    fn empty_ignore_patterns_changes_nothing() {
        let td = TestDir::new();
        td.create_file("a.rs");
        td.create_file("b.txt");

        let walker_plain = Walker::new(td.path());
        let walker_empty = Walker::new(td.path()).with_ignore_patterns(&[]);

        let plain = sorted_relative(td.path(), &walker_plain.collect_paths());
        let empty = sorted_relative(td.path(), &walker_empty.collect_paths());

        assert_eq!(plain, empty, "empty patterns should not change behavior");
    }

    // ----- Worktree boundary exclusion tests -----

    #[test]
    fn skips_nested_git_directory() {
        let td = TestDir::new();
        // Root has a .git directory (making it a repo)
        fs::create_dir(td.path().join(".git")).unwrap();
        td.create_file("src/main.rs");
        // Nested repo with its own .git directory
        td.create_file("libs/nested-repo/lib.rs");
        fs::create_dir(td.path().join("libs/nested-repo/.git")).unwrap();
        td.create_file("libs/nested-repo/src/inner.rs");

        let walker = Walker::new(td.path());
        let paths = walker.collect_paths();
        let rel = sorted_relative(td.path(), &paths);

        assert!(
            rel.contains(&"src/main.rs".to_string()),
            "root src/main.rs should be present, got: {rel:?}"
        );
        assert!(
            !rel.iter().any(|p| p.starts_with("libs/nested-repo")),
            "nested repo should be skipped due to .git directory boundary, got: {rel:?}"
        );
    }

    #[test]
    fn skips_nested_git_file_worktree() {
        let td = TestDir::new();
        // Root has a .git directory
        fs::create_dir(td.path().join(".git")).unwrap();
        td.create_file("src/main.rs");
        // Linked worktree has a .git *file* (not directory)
        td.create_file("libs/linked-wt/lib.rs");
        fs::write(
            td.path().join("libs/linked-wt/.git"),
            "gitdir: /some/path/.git/worktrees/linked-wt",
        )
        .unwrap();
        td.create_file("libs/linked-wt/src/inner.rs");

        let walker = Walker::new(td.path());
        let paths = walker.collect_paths();
        let rel = sorted_relative(td.path(), &paths);

        assert!(
            rel.contains(&"src/main.rs".to_string()),
            "root src/main.rs should be present, got: {rel:?}"
        );
        assert!(
            !rel.iter().any(|p| p.starts_with("libs/linked-wt")),
            "linked worktree should be skipped due to .git file boundary, got: {rel:?}"
        );
    }

    #[test]
    fn root_git_dir_is_not_skipped() {
        let td = TestDir::new();
        // Root has a .git directory -- this must NOT cause the root to be skipped
        fs::create_dir(td.path().join(".git")).unwrap();
        td.create_file("src/main.rs");
        td.create_file("lib.rs");

        let walker = Walker::new(td.path());
        let paths = walker.collect_paths();
        let rel = sorted_relative(td.path(), &paths);

        assert!(
            rel.contains(&"src/main.rs".to_string()),
            "src/main.rs should be present, got: {rel:?}"
        );
        assert!(
            rel.contains(&"lib.rs".to_string()),
            "lib.rs should be present, got: {rel:?}"
        );
    }

    #[test]
    fn worktree_exclusion_coexists_with_default_exclusions() {
        let td = TestDir::new();
        fs::create_dir(td.path().join(".git")).unwrap();
        td.create_file("src/main.rs");
        // Default exclusion: node_modules
        td.create_file("node_modules/pkg/index.js");
        // Worktree boundary: nested .git
        td.create_file("sub/nested-repo/lib.rs");
        fs::create_dir(td.path().join("sub/nested-repo/.git")).unwrap();

        let walker = Walker::new(td.path());
        let paths = walker.collect_paths();
        let rel = sorted_relative(td.path(), &paths);

        assert!(
            rel.contains(&"src/main.rs".to_string()),
            "src/main.rs should be present, got: {rel:?}"
        );
        assert!(
            !rel.iter().any(|p| p.starts_with("node_modules")),
            "node_modules should still be excluded, got: {rel:?}"
        );
        assert!(
            !rel.iter().any(|p| p.starts_with("sub/nested-repo")),
            "nested repo should be excluded by worktree boundary, got: {rel:?}"
        );
    }

    #[test]
    fn worktree_boundary_parallel_matches_sequential() {
        let td = TestDir::new();
        fs::create_dir(td.path().join(".git")).unwrap();
        td.create_file("a.rs");
        td.create_file("sub/b.rs");
        // Nested repo should be skipped in both modes
        td.create_file("nested/repo/lib.rs");
        fs::create_dir(td.path().join("nested/repo/.git")).unwrap();

        let walker = Walker::new(td.path());
        let mut seq = sorted_relative(td.path(), &walker.collect_paths());
        let mut par = sorted_relative(td.path(), &walker.collect_paths_parallel());
        seq.sort();
        par.sort();

        assert_eq!(
            seq, par,
            "sequential and parallel walks should match with worktree boundary exclusion"
        );
        assert!(
            !seq.iter().any(|p| p.starts_with("nested/repo")),
            "nested repo should be excluded in both modes, got: {seq:?}"
        );
    }
}
