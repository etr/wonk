//! File walker with gitignore support and default exclusions.
//!
//! Wraps the `ignore` crate's `WalkBuilder` to provide a file walker that:
//! - Respects `.gitignore` rules
//! - Skips common build/dependency directories by default
//! - Skips hidden files/directories except `.github`
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

/// A file-system walker that respects `.gitignore` and applies default
/// exclusions.
pub struct Walker {
    root: PathBuf,
    threads: usize,
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
        }
    }

    /// Set the number of threads used for parallel walking.
    ///
    /// A value of `0` (the default) lets the `ignore` crate choose.
    pub fn threads(mut self, n: usize) -> Self {
        self.threads = n;
        self
    }

    /// Build the underlying `WalkBuilder` with all our configuration applied.
    fn make_builder(&self) -> WalkBuilder {
        let mut builder = WalkBuilder::new(&self.root);

        // Let the ignore crate handle .gitignore, .ignore, etc.
        builder.standard_filters(true);

        // We disable the built-in hidden filter because we need a more
        // nuanced policy (skip hidden except for allowlisted names).
        builder.hidden(false);

        // Build overrides that negate (exclude) the default directories.
        // In the overrides system, a glob WITHOUT `!` means "include only",
        // and a glob WITH `!` means "exclude".  We want to exclude these dirs.
        let mut overrides = OverrideBuilder::new(&self.root);
        for dir in DEFAULT_EXCLUSIONS {
            // The `!` prefix in override globs means "exclude this pattern".
            let pattern = format!("!{dir}/");
            overrides
                .add(&pattern)
                .expect("default exclusion pattern should be valid");
        }
        builder.overrides(
            overrides.build().expect("override builder should succeed"),
        );

        // Custom filter: skip hidden entries (name starts with `.`) unless
        // they appear in the allowlist.
        builder.filter_entry(|entry| {
            let name = entry.file_name().to_string_lossy();
            if name.starts_with('.') {
                // The root entry itself (depth 0) always passes through.
                if entry.depth() == 0 {
                    return true;
                }
                return HIDDEN_ALLOWLIST.iter().any(|a| *a == &*name);
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
}
