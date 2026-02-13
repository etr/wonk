//! File watcher with debounced events for incremental re-indexing.
//!
//! Wraps `notify-debouncer-mini` to produce debounced filesystem events and
//! feeds them into a `crossbeam-channel`.  The daemon event loop receives
//! events from the channel, filters them through gitignore / default
//! exclusion rules, classifies each event as Created / Modified / Deleted,
//! and dispatches to a caller-supplied handler.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use crossbeam_channel::{Receiver, Sender};
use notify_debouncer_mini::notify::RecursiveMode;
use notify_debouncer_mini::{DebouncedEvent, DebounceEventResult, new_debouncer};

// ---------------------------------------------------------------------------
// File event types
// ---------------------------------------------------------------------------

/// A classified filesystem event ready for the re-indexer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileEvent {
    /// A new file was created (or appeared via rename-to).
    Created(PathBuf),
    /// An existing file was modified.
    Modified(PathBuf),
    /// A file was deleted (or disappeared via rename-from).
    Deleted(PathBuf),
}

impl FileEvent {
    /// Return the path associated with this event.
    pub fn path(&self) -> &Path {
        match self {
            FileEvent::Created(p) | FileEvent::Modified(p) | FileEvent::Deleted(p) => p,
        }
    }
}

/// Classify a debounced event by checking whether the path still exists on
/// disk.  Since `notify-debouncer-mini` only reports `Any` / `AnyContinuous`
/// without distinguishing create/modify/delete, we probe the filesystem:
///
/// - Path exists and has metadata -> `Created` or `Modified` (we treat newly
///   seen files as Created; for the re-indexer both map to "upsert", so the
///   distinction is informational).
/// - Path does not exist -> `Deleted`.
fn classify_event(event: &DebouncedEvent) -> FileEvent {
    if event.path.exists() {
        // We cannot distinguish create from modify purely from the debounced
        // event.  Both will trigger a re-index (upsert).  We use `Modified`
        // as the default since it is the common case; callers who need to
        // know about brand-new files can check their own index state.
        FileEvent::Modified(event.path.clone())
    } else {
        FileEvent::Deleted(event.path.clone())
    }
}

// ---------------------------------------------------------------------------
// Default exclusion / filtering
// ---------------------------------------------------------------------------

/// Directories that are always excluded from watching, matching walker.rs.
const DEFAULT_EXCLUSIONS: &[&str] = &[
    "node_modules",
    "vendor",
    "target",
    "build",
    "dist",
    "__pycache__",
    ".venv",
];

/// Hidden directory names that are allowed through the filter.
const HIDDEN_ALLOWLIST: &[&str] = &[".github"];

/// Determine whether a filesystem event for `path` should be processed.
///
/// Returns `false` for paths that fall inside default-excluded directories,
/// hidden directories (unless allowlisted), the `.git` directory itself,
/// or nested worktree boundaries (directories containing a `.git` entry
/// that are not the repo root).
///
/// `path` must be relative to `repo_root`.  `repo_root` is the absolute
/// path to the repository root; it is used to probe the filesystem for
/// nested `.git` entries without treating the root's own `.git` as a
/// boundary.
///
/// This mirrors the filtering logic in `walker.rs` so the watcher and the
/// initial walker agree on which files belong to the index.
pub fn should_process(path: &Path, repo_root: &Path) -> bool {
    for component in path.components() {
        let name = component.as_os_str().to_string_lossy();

        // Skip the `.git` directory itself (internal git data).
        if name == ".git" {
            return false;
        }

        // Check default exclusion directories.
        if DEFAULT_EXCLUSIONS.iter().any(|exc| *exc == &*name) {
            return false;
        }

        // Check hidden directories/files (starting with `.`), excluding
        // allowlisted names.
        if name.starts_with('.') && !HIDDEN_ALLOWLIST.iter().any(|a| *a == &*name) {
            if let std::path::Component::Normal(_) = component {
                return false;
            }
        }
    }

    // Worktree boundary check: walk ancestor directories between
    // repo_root and the event path.  If any directory along the way
    // contains a `.git` entry (file or directory), the path is inside a
    // nested worktree or sub-repository and should be discarded.
    let mut ancestor = repo_root.to_path_buf();
    for component in path.components() {
        if let std::path::Component::Normal(seg) = component {
            ancestor.push(seg);
            if ancestor.join(".git").exists() {
                return false;
            }
        }
    }

    true
}

// ---------------------------------------------------------------------------
// FileWatcher
// ---------------------------------------------------------------------------

/// Wraps `notify-debouncer-mini` and feeds classified, filtered events into a
/// crossbeam channel.
pub struct FileWatcher {
    /// The debouncer must be kept alive; dropping it stops the watcher.
    _debouncer: notify_debouncer_mini::Debouncer<notify_debouncer_mini::notify::RecommendedWatcher>,
}

impl FileWatcher {
    /// Create a new file watcher for `repo_root` with the given debounce
    /// window in milliseconds.
    ///
    /// Returns the watcher (which must be kept alive) and a receiver for
    /// batches of classified `FileEvent`s.  Events for paths that fail the
    /// `should_process` filter are silently dropped before being sent.
    pub fn new(
        repo_root: &Path,
        debounce_ms: u64,
    ) -> Result<(Self, Receiver<Vec<FileEvent>>)> {
        let (tx, rx): (Sender<Vec<FileEvent>>, Receiver<Vec<FileEvent>>) =
            crossbeam_channel::unbounded();

        let repo_root_buf = repo_root.to_path_buf();

        let mut debouncer = new_debouncer(
            Duration::from_millis(debounce_ms),
            move |res: DebounceEventResult| {
                if let Ok(events) = res {
                    let file_events: Vec<FileEvent> = events
                        .iter()
                        .filter_map(|ev| {
                            // Make the path relative to repo root for filtering,
                            // but keep the absolute path in the event.
                            let rel = ev.path.strip_prefix(&repo_root_buf).unwrap_or(&ev.path);
                            if should_process(rel, &repo_root_buf) {
                                Some(classify_event(ev))
                            } else {
                                None
                            }
                        })
                        .collect();

                    if !file_events.is_empty() {
                        let _ = tx.send(file_events);
                    }
                }
            },
        )
        .context("creating debounced file watcher")?;

        debouncer
            .watcher()
            .watch(repo_root, RecursiveMode::Recursive)
            .with_context(|| {
                format!(
                    "starting recursive watch on {}",
                    repo_root.display()
                )
            })?;

        Ok((FileWatcher { _debouncer: debouncer }, rx))
    }
}

// ---------------------------------------------------------------------------
// Daemon event loop
// ---------------------------------------------------------------------------

/// Run the daemon event loop, receiving batches of `FileEvent` from the
/// channel and dispatching each batch to `handler`.
///
/// The loop exits when `shutdown` is set to `true` (e.g. by a signal handler)
/// or when the channel is disconnected (watcher dropped).
///
/// `handler` receives a slice of events per batch.  It is expected to perform
/// incremental re-indexing (upsert for Created/Modified, removal for Deleted).
pub fn run_event_loop<F>(
    rx: &Receiver<Vec<FileEvent>>,
    shutdown: &Arc<AtomicBool>,
    mut handler: F,
) where
    F: FnMut(&[FileEvent]),
{
    // Use a short timeout so we can check the shutdown flag periodically.
    let poll_timeout = Duration::from_millis(200);

    loop {
        if shutdown.load(Ordering::Relaxed) {
            break;
        }

        match rx.recv_timeout(poll_timeout) {
            Ok(events) => {
                if !events.is_empty() {
                    handler(&events);
                }
            }
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                // No events; loop back and check shutdown flag.
                continue;
            }
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                // Watcher was dropped or channel closed.
                break;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    // ---- should_process filtering tests ----

    // For pure path-component tests (no filesystem boundary checks), we pass
    // an empty path as repo_root so the worktree boundary probe is a no-op.
    const NO_ROOT: &str = "";

    #[test]
    fn test_should_process_normal_source_file() {
        assert!(should_process(Path::new("src/main.rs"), Path::new(NO_ROOT)));
    }

    #[test]
    fn test_should_process_nested_source_file() {
        assert!(should_process(Path::new("src/utils/helpers.rs"), Path::new(NO_ROOT)));
    }

    #[test]
    fn test_should_process_rejects_node_modules() {
        assert!(!should_process(Path::new("node_modules/pkg/index.js"), Path::new(NO_ROOT)));
    }

    #[test]
    fn test_should_process_rejects_vendor() {
        assert!(!should_process(Path::new("vendor/lib.go"), Path::new(NO_ROOT)));
    }

    #[test]
    fn test_should_process_rejects_target() {
        assert!(!should_process(Path::new("target/debug/binary"), Path::new(NO_ROOT)));
    }

    #[test]
    fn test_should_process_rejects_build() {
        assert!(!should_process(Path::new("build/output.js"), Path::new(NO_ROOT)));
    }

    #[test]
    fn test_should_process_rejects_dist() {
        assert!(!should_process(Path::new("dist/bundle.js"), Path::new(NO_ROOT)));
    }

    #[test]
    fn test_should_process_rejects_pycache() {
        assert!(!should_process(Path::new("__pycache__/module.pyc"), Path::new(NO_ROOT)));
    }

    #[test]
    fn test_should_process_rejects_venv() {
        assert!(!should_process(Path::new(".venv/bin/python"), Path::new(NO_ROOT)));
    }

    #[test]
    fn test_should_process_rejects_git_dir() {
        assert!(!should_process(Path::new(".git/objects/pack/abc"), Path::new(NO_ROOT)));
    }

    #[test]
    fn test_should_process_rejects_hidden_directory() {
        assert!(!should_process(Path::new(".hidden/secret.txt"), Path::new(NO_ROOT)));
    }

    #[test]
    fn test_should_process_rejects_hidden_config_dir() {
        assert!(!should_process(Path::new(".config/settings.toml"), Path::new(NO_ROOT)));
    }

    #[test]
    fn test_should_process_allows_github_directory() {
        assert!(should_process(Path::new(".github/workflows/ci.yml"), Path::new(NO_ROOT)));
    }

    #[test]
    fn test_should_process_rejects_deep_exclusion() {
        // Even if node_modules is nested under a normal dir, it should
        // still be caught.
        assert!(!should_process(
            Path::new("packages/foo/node_modules/bar/index.js"),
            Path::new(NO_ROOT),
        ));
    }

    #[test]
    fn test_should_process_rejects_deep_hidden() {
        assert!(!should_process(Path::new("src/.cache/data"), Path::new(NO_ROOT)));
    }

    #[test]
    fn test_should_process_top_level_file() {
        assert!(should_process(Path::new("README.md"), Path::new(NO_ROOT)));
    }

    #[test]
    fn test_should_process_empty_path() {
        // An empty path has no components to reject.
        assert!(should_process(Path::new(""), Path::new(NO_ROOT)));
    }

    // ---- should_process worktree boundary tests ----

    #[test]
    fn test_should_process_rejects_nested_worktree_git_dir() {
        use std::fs;
        let dir = tempfile::tempdir().unwrap();
        // Set up a nested repo with a .git directory
        fs::create_dir_all(dir.path().join("libs/nested-repo/.git")).unwrap();
        fs::create_dir_all(dir.path().join("libs/nested-repo/src")).unwrap();
        fs::write(dir.path().join("libs/nested-repo/src/lib.rs"), "").unwrap();

        // Path inside nested repo should be rejected
        assert!(
            !should_process(
                Path::new("libs/nested-repo/src/lib.rs"),
                dir.path()
            ),
            "events inside nested worktree boundary (.git dir) should be rejected"
        );
    }

    #[test]
    fn test_should_process_rejects_nested_worktree_git_file() {
        use std::fs;
        let dir = tempfile::tempdir().unwrap();
        // Set up a linked worktree with a .git *file*
        fs::create_dir_all(dir.path().join("libs/linked-wt/src")).unwrap();
        fs::write(
            dir.path().join("libs/linked-wt/.git"),
            "gitdir: /some/path/.git/worktrees/linked-wt",
        )
        .unwrap();
        fs::write(dir.path().join("libs/linked-wt/src/lib.rs"), "").unwrap();

        assert!(
            !should_process(
                Path::new("libs/linked-wt/src/lib.rs"),
                dir.path()
            ),
            "events inside nested worktree boundary (.git file) should be rejected"
        );
    }

    #[test]
    fn test_should_process_allows_own_repo_files() {
        use std::fs;
        let dir = tempfile::tempdir().unwrap();
        // Repo root has .git dir -- this is the root's own .git, not a boundary
        fs::create_dir(dir.path().join(".git")).unwrap();
        fs::create_dir(dir.path().join("src")).unwrap();
        fs::write(dir.path().join("src/main.rs"), "").unwrap();

        assert!(
            should_process(Path::new("src/main.rs"), dir.path()),
            "events from the repo's own files should be processed"
        );
    }

    #[test]
    fn test_should_process_allows_top_level_file_with_nested_worktree() {
        use std::fs;
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join(".git")).unwrap();
        fs::write(dir.path().join("README.md"), "").unwrap();
        // Also has a nested worktree
        fs::create_dir_all(dir.path().join("libs/nested/.git")).unwrap();

        assert!(
            should_process(Path::new("README.md"), dir.path()),
            "top-level files should pass even when nested worktree exists"
        );
    }

    #[test]
    fn test_should_process_rejects_deeply_nested_worktree() {
        use std::fs;
        let dir = tempfile::tempdir().unwrap();
        // Deeply nested worktree boundary
        fs::create_dir_all(dir.path().join("a/b/c/nested/.git")).unwrap();
        fs::create_dir_all(dir.path().join("a/b/c/nested/deep/src")).unwrap();
        fs::write(dir.path().join("a/b/c/nested/deep/src/lib.rs"), "").unwrap();

        assert!(
            !should_process(
                Path::new("a/b/c/nested/deep/src/lib.rs"),
                dir.path()
            ),
            "events from deeply nested worktree boundary should be rejected"
        );
    }

    #[test]
    fn test_should_process_preserves_git_dir_rejection() {
        use std::fs;
        let dir = tempfile::tempdir().unwrap();
        // Existing behavior: .git dir in path components should still be rejected
        assert!(
            !should_process(
                Path::new(".git/objects/pack/abc"),
                dir.path()
            ),
            ".git directory events should still be rejected (existing behavior)"
        );
    }

    // ---- classify_event tests ----

    #[test]
    fn test_classify_event_existing_file() {
        // Use a path that we know exists (the test binary or Cargo.toml).
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
        let ev = DebouncedEvent {
            path: path.clone(),
            kind: notify_debouncer_mini::DebouncedEventKind::Any,
        };
        let result = classify_event(&ev);
        assert_eq!(result, FileEvent::Modified(path));
    }

    #[test]
    fn test_classify_event_nonexistent_file() {
        let path = PathBuf::from("/tmp/definitely_does_not_exist_wonk_test_xyz.rs");
        let ev = DebouncedEvent {
            path: path.clone(),
            kind: notify_debouncer_mini::DebouncedEventKind::Any,
        };
        let result = classify_event(&ev);
        assert_eq!(result, FileEvent::Deleted(path));
    }

    // ---- FileEvent::path tests ----

    #[test]
    fn test_file_event_path_created() {
        let p = PathBuf::from("src/foo.rs");
        let ev = FileEvent::Created(p.clone());
        assert_eq!(ev.path(), p.as_path());
    }

    #[test]
    fn test_file_event_path_modified() {
        let p = PathBuf::from("src/bar.rs");
        let ev = FileEvent::Modified(p.clone());
        assert_eq!(ev.path(), p.as_path());
    }

    #[test]
    fn test_file_event_path_deleted() {
        let p = PathBuf::from("src/baz.rs");
        let ev = FileEvent::Deleted(p.clone());
        assert_eq!(ev.path(), p.as_path());
    }

    // ---- run_event_loop tests ----

    #[test]
    fn test_run_event_loop_processes_events() {
        let (tx, rx) = crossbeam_channel::unbounded();
        let shutdown = Arc::new(AtomicBool::new(false));

        // Send a batch of events, then signal shutdown.
        let events = vec![
            FileEvent::Modified(PathBuf::from("src/main.rs")),
            FileEvent::Deleted(PathBuf::from("old_file.rs")),
        ];
        tx.send(events.clone()).unwrap();

        // Signal shutdown after sending events so the loop will process
        // the batch and then exit.
        let shutdown_clone = Arc::clone(&shutdown);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            shutdown_clone.store(true, Ordering::Relaxed);
        });

        let mut received = Vec::new();
        run_event_loop(&rx, &shutdown, |batch| {
            received.extend_from_slice(batch);
        });

        assert_eq!(received, events);
    }

    #[test]
    fn test_run_event_loop_exits_on_shutdown() {
        let (_tx, rx) = crossbeam_channel::unbounded::<Vec<FileEvent>>();
        let shutdown = Arc::new(AtomicBool::new(true)); // Already set.

        let mut called = false;
        run_event_loop(&rx, &shutdown, |_| {
            called = true;
        });

        assert!(!called, "handler should not be called when shutdown is immediate");
    }

    #[test]
    fn test_run_event_loop_exits_on_disconnect() {
        let (tx, rx) = crossbeam_channel::unbounded::<Vec<FileEvent>>();
        let shutdown = Arc::new(AtomicBool::new(false));

        // Drop the sender to disconnect the channel.
        drop(tx);

        let mut called = false;
        run_event_loop(&rx, &shutdown, |_| {
            called = true;
        });

        assert!(!called, "handler should not be called when channel is disconnected");
    }

    #[test]
    fn test_run_event_loop_skips_empty_batches() {
        let (tx, rx) = crossbeam_channel::unbounded();
        let shutdown = Arc::new(AtomicBool::new(false));

        // Send an empty batch followed by a non-empty one.
        tx.send(vec![]).unwrap();
        tx.send(vec![FileEvent::Modified(PathBuf::from("a.rs"))]).unwrap();

        let shutdown_clone = Arc::clone(&shutdown);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            shutdown_clone.store(true, Ordering::Relaxed);
        });

        let mut received = Vec::new();
        run_event_loop(&rx, &shutdown, |batch| {
            received.extend_from_slice(batch);
        });

        assert_eq!(received, vec![FileEvent::Modified(PathBuf::from("a.rs"))]);
    }

    // ---- Integration: FileWatcher with real filesystem ----

    #[test]
    fn test_file_watcher_creates_and_receives_events() {
        use std::fs;
        let dir = tempfile::tempdir().unwrap();
        let (watcher, rx) = FileWatcher::new(dir.path(), 300).unwrap();

        // Create a file inside the watched directory.
        let file_path = dir.path().join("test_file.rs");
        fs::write(&file_path, "fn main() {}").unwrap();

        // Wait for debounced event (300ms debounce + some slack).
        let events = rx.recv_timeout(Duration::from_secs(5));
        assert!(
            events.is_ok(),
            "should receive events after creating a file"
        );

        let events = events.unwrap();
        assert!(!events.is_empty(), "event batch should not be empty");
        // The file exists so it should be Modified (our classification).
        assert!(
            events.iter().any(|e| matches!(e, FileEvent::Modified(p) if p == &file_path)),
            "should see Modified event for the created file, got: {events:?}"
        );

        // Keep watcher alive for the duration of the test.
        drop(watcher);
    }

    #[test]
    fn test_file_watcher_filters_excluded_paths() {
        use std::fs;
        let dir = tempfile::tempdir().unwrap();

        // Create node_modules before starting the watcher.
        fs::create_dir_all(dir.path().join("node_modules/pkg")).unwrap();
        fs::create_dir_all(dir.path().join("src")).unwrap();

        let (watcher, rx) = FileWatcher::new(dir.path(), 300).unwrap();

        // Write to an excluded directory.
        fs::write(
            dir.path().join("node_modules/pkg/index.js"),
            "module.exports = {}",
        )
        .unwrap();

        // Also write to a normal directory to ensure we get at least that event.
        std::thread::sleep(Duration::from_millis(50));
        fs::write(dir.path().join("src/lib.rs"), "pub fn hello() {}").unwrap();

        // Collect events for a reasonable window.
        let mut all_events = Vec::new();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            match rx.recv_timeout(Duration::from_millis(500)) {
                Ok(batch) => all_events.extend(batch),
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                    if !all_events.is_empty() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }

        // We should see the src/lib.rs event but not node_modules.
        let has_lib = all_events
            .iter()
            .any(|e| e.path().to_string_lossy().contains("lib.rs"));
        let has_node_modules = all_events
            .iter()
            .any(|e| e.path().to_string_lossy().contains("node_modules"));

        assert!(has_lib, "should receive event for src/lib.rs, got: {all_events:?}");
        assert!(
            !has_node_modules,
            "should NOT receive events for node_modules, got: {all_events:?}"
        );

        drop(watcher);
    }

    #[test]
    fn test_file_watcher_filters_nested_worktree_events() {
        use std::fs;
        let dir = tempfile::tempdir().unwrap();

        // Set up directory structure before starting watcher.
        fs::create_dir_all(dir.path().join("src")).unwrap();
        // Nested worktree with a .git directory boundary.
        fs::create_dir_all(dir.path().join("libs/nested/.git")).unwrap();
        fs::create_dir_all(dir.path().join("libs/nested/src")).unwrap();

        let (watcher, rx) = FileWatcher::new(dir.path(), 300).unwrap();

        // Write to a file inside the nested worktree boundary.
        fs::write(
            dir.path().join("libs/nested/src/lib.rs"),
            "pub fn nested() {}",
        )
        .unwrap();

        // Also write to a normal directory to ensure we get at least that event.
        std::thread::sleep(Duration::from_millis(50));
        fs::write(dir.path().join("src/main.rs"), "fn main() {}").unwrap();

        // Collect events for a reasonable window.
        let mut all_events = Vec::new();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            match rx.recv_timeout(Duration::from_millis(500)) {
                Ok(batch) => all_events.extend(batch),
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                    if !all_events.is_empty() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }

        // We should see the src/main.rs event but NOT libs/nested/src/lib.rs.
        let has_main = all_events
            .iter()
            .any(|e| e.path().to_string_lossy().contains("main.rs"));
        let has_nested = all_events
            .iter()
            .any(|e| e.path().to_string_lossy().contains("libs/nested"));

        assert!(
            has_main,
            "should receive event for src/main.rs, got: {all_events:?}"
        );
        assert!(
            !has_nested,
            "should NOT receive events for files inside nested worktree boundary, got: {all_events:?}"
        );

        drop(watcher);
    }
}
