//! Background daemon for file watching and incremental indexing.
//!
//! Provides daemon spawning via double-fork, PID file management,
//! single-instance enforcement, graceful shutdown via SIGTERM, and
//! daemon status reporting via the `daemon_status` SQLite table.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use fork::{Fork, fork, setsid};
use rusqlite::Connection;
use signal_hook::flag;

use crate::db;
use crate::embedding::OllamaClient;
use crate::pipeline;
use crate::watcher::{self, FileWatcher};

// ---------------------------------------------------------------------------
// Timestamp helper
// ---------------------------------------------------------------------------

/// Return the current Unix epoch timestamp in seconds.
fn now_epoch() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

// ---------------------------------------------------------------------------
// Daemon status table helpers
// ---------------------------------------------------------------------------

/// Aggregated daemon information read from the `daemon_status` table.
#[derive(Debug, Clone, Default)]
pub struct DaemonInfo {
    pub pid: Option<String>,
    pub state: Option<String>,
    pub uptime_start: Option<String>,
    pub last_activity: Option<String>,
    pub files_queued: Option<String>,
    pub last_error: Option<String>,
    pub heartbeat: Option<String>,
    pub embedding_last_activity: Option<String>,
    pub embedding_files_count: Option<String>,
}

/// Write a single key/value pair into the `daemon_status` table.
///
/// Uses `INSERT OR REPLACE` so the call is idempotent.
pub fn write_status(conn: &Connection, key: &str, value: &str) -> Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO daemon_status (key, value, updated_at) VALUES (?1, ?2, ?3)",
        rusqlite::params![key, value, now_epoch()],
    )
    .with_context(|| format!("writing daemon_status key '{key}'"))?;
    Ok(())
}

/// Read a single value from the `daemon_status` table.
pub fn read_status(conn: &Connection, key: &str) -> Result<Option<String>> {
    let mut stmt = conn
        .prepare("SELECT value FROM daemon_status WHERE key = ?1")
        .context("preparing read_status query")?;
    let mut rows = stmt
        .query_map(rusqlite::params![key], |row| row.get::<_, String>(0))
        .context("executing read_status query")?;
    match rows.next() {
        Some(Ok(val)) => Ok(Some(val)),
        Some(Err(e)) => Err(e).context("reading daemon_status value"),
        None => Ok(None),
    }
}

/// Write startup status: pid, state=running, uptime_start.
pub fn write_startup_status(conn: &Connection, pid: u32) -> Result<()> {
    let now = now_epoch().to_string();
    write_status(conn, "pid", &pid.to_string())?;
    write_status(conn, "state", "running")?;
    write_status(conn, "uptime_start", &now)?;
    write_status(conn, "heartbeat", &now)?;
    Ok(())
}

/// Update the `last_activity` timestamp (call on each index update).
pub fn update_activity(conn: &Connection) -> Result<()> {
    write_status(conn, "last_activity", &now_epoch().to_string())
}

/// Update the `files_queued` count (call when processing batches).
pub fn update_queue_depth(conn: &Connection, count: usize) -> Result<()> {
    write_status(conn, "files_queued", &count.to_string())
}

/// Write the last error message.
pub fn write_error(conn: &Connection, error_msg: &str) -> Result<()> {
    write_status(conn, "last_error", error_msg)
}

/// Update the heartbeat timestamp (call periodically, e.g. every 30s).
pub fn write_heartbeat(conn: &Connection) -> Result<()> {
    write_status(conn, "heartbeat", &now_epoch().to_string())
}

/// Update embedding activity status (call after each embedding re-index).
pub fn update_embedding_activity(conn: &Connection, files_count: usize) -> Result<()> {
    write_status(conn, "embedding_last_activity", &now_epoch().to_string())?;
    write_status(conn, "embedding_files_count", &files_count.to_string())?;
    Ok(())
}

/// Clear all rows from the `daemon_status` table (call on clean shutdown).
pub fn clear_status(conn: &Connection) -> Result<()> {
    conn.execute("DELETE FROM daemon_status", [])
        .context("clearing daemon_status table")?;
    Ok(())
}

/// Read all daemon status rows into a [`DaemonInfo`] struct.
pub fn read_all_status(conn: &Connection) -> Result<DaemonInfo> {
    let mut stmt = conn
        .prepare("SELECT key, value FROM daemon_status")
        .context("preparing read_all_status query")?;
    let rows = stmt
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .context("executing read_all_status query")?;

    let mut map = HashMap::new();
    for row in rows {
        let (k, v) = row.context("reading daemon_status row")?;
        map.insert(k, v);
    }

    Ok(DaemonInfo {
        pid: map.remove("pid"),
        state: map.remove("state"),
        uptime_start: map.remove("uptime_start"),
        last_activity: map.remove("last_activity"),
        files_queued: map.remove("files_queued"),
        last_error: map.remove("last_error"),
        heartbeat: map.remove("heartbeat"),
        embedding_last_activity: map.remove("embedding_last_activity"),
        embedding_files_count: map.remove("embedding_files_count"),
    })
}

// ---------------------------------------------------------------------------
// PID file path
// ---------------------------------------------------------------------------

/// Returns the path to `daemon.pid` alongside `index.db`.
pub fn pid_file_path(index_dir: &Path) -> PathBuf {
    index_dir.join("daemon.pid")
}

// ---------------------------------------------------------------------------
// PID file management
// ---------------------------------------------------------------------------

/// Check whether a daemon is currently running for the given index directory.
///
/// Returns `true` if a PID file exists **and** the process it references is
/// still alive.
pub fn is_running(index_dir: &Path) -> bool {
    let pid_path = pid_file_path(index_dir);
    match fs::read_to_string(&pid_path) {
        Ok(contents) => {
            if let Ok(pid) = contents.trim().parse::<u32>() {
                process_alive(pid)
            } else {
                false
            }
        }
        Err(_) => false,
    }
}

/// Remove a stale PID file if the referenced process is no longer running.
///
/// If the PID file exists and points to a live process, this is a no-op.
/// If the PID file exists but the process is gone, the file is removed.
/// If the PID file does not exist, this is a no-op.
pub fn check_stale_pid(index_dir: &Path) -> Result<()> {
    let pid_path = pid_file_path(index_dir);
    match fs::read_to_string(&pid_path) {
        Ok(contents) => {
            let pid = contents
                .trim()
                .parse::<u32>()
                .context("parsing PID from daemon.pid")?;
            if !process_alive(pid) {
                fs::remove_file(&pid_path)
                    .with_context(|| format!("removing stale PID file {}", pid_path.display()))?;
            }
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).context("reading daemon.pid"),
    }
}

/// Write the current process's PID to `daemon.pid`.
pub fn write_pid(index_dir: &Path) -> Result<()> {
    let pid_path = pid_file_path(index_dir);
    fs::create_dir_all(index_dir)
        .with_context(|| format!("creating index directory {}", index_dir.display()))?;
    fs::write(&pid_path, format!("{}\n", process::id()))
        .with_context(|| format!("writing PID file {}", pid_path.display()))?;
    Ok(())
}

/// Remove the PID file if it exists.
pub fn remove_pid(index_dir: &Path) -> Result<()> {
    let pid_path = pid_file_path(index_dir);
    match fs::remove_file(&pid_path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("removing PID file {}", pid_path.display())),
    }
}

// ---------------------------------------------------------------------------
// Process existence check
// ---------------------------------------------------------------------------

/// Check whether a process with the given PID is alive.
///
/// Uses `kill(pid, 0)` which checks for process existence without actually
/// sending a signal.
fn process_alive(pid: u32) -> bool {
    // SAFETY: kill with signal 0 is a standard POSIX existence check.
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

// ---------------------------------------------------------------------------
// Daemonize (double-fork)
// ---------------------------------------------------------------------------

/// Daemonize the current process via the classic double-fork technique.
///
/// After this function returns `Ok(())`, the caller is the final grandchild
/// process running in its own session, fully detached from the original
/// terminal.
///
/// # Errors
///
/// Returns an error if any fork or setsid call fails.
pub fn daemonize() -> Result<()> {
    // First fork: parent exits, child continues.
    match fork().map_err(|e| anyhow::anyhow!("first fork failed: {e}"))? {
        Fork::Parent(_child_pid) => {
            // Parent exits successfully.
            process::exit(0);
        }
        Fork::Child => {
            // Create a new session so we detach from the controlling terminal.
            setsid().map_err(|e| anyhow::anyhow!("setsid failed: {e}"))?;

            // Second fork: ensures we can never reacquire a controlling terminal.
            match fork().map_err(|e| anyhow::anyhow!("second fork failed: {e}"))? {
                Fork::Parent(_grandchild_pid) => {
                    // Intermediate child exits.
                    process::exit(0);
                }
                Fork::Child => {
                    // This is the final daemon process.
                    Ok(())
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Signal handling
// ---------------------------------------------------------------------------

/// Register a SIGTERM handler that sets an atomic flag.
///
/// Returns the flag so the caller can poll it in an event loop.
pub fn register_signal_handler() -> Result<Arc<AtomicBool>> {
    let term = Arc::new(AtomicBool::new(false));
    flag::register(signal_hook::consts::SIGTERM, Arc::clone(&term))
        .context("registering SIGTERM handler")?;
    // Also handle SIGINT for interactive testing / development.
    flag::register(signal_hook::consts::SIGINT, Arc::clone(&term))
        .context("registering SIGINT handler")?;
    Ok(term)
}

// ---------------------------------------------------------------------------
// Embedding worker helpers
// ---------------------------------------------------------------------------

/// Drain any pending batches from `rx` and merge them with `first`,
/// deduplicating file paths.  This coalesces rapid successive file change
/// notifications so the embedding worker processes each file at most once.
pub fn coalesce_file_batches(
    first: Vec<String>,
    rx: &crossbeam_channel::Receiver<Vec<String>>,
) -> Vec<String> {
    let mut all: Vec<String> = first;
    while let Ok(batch) = rx.try_recv() {
        all.extend(batch);
    }
    all.sort();
    all.dedup();
    all
}

// ---------------------------------------------------------------------------
// Spawn daemon (main entry point)
// ---------------------------------------------------------------------------

/// Spawn the daemon for the given repository.
///
/// This function:
/// 1. Resolves the index directory.
/// 2. Checks for stale PID files.
/// 3. Enforces single-instance (bails if a daemon is already running).
/// 4. Performs double-fork daemonization.
/// 5. Writes the PID file.
/// 6. Registers a SIGTERM handler.
/// 7. Runs the event loop (placeholder: waits for SIGTERM).
/// 8. On shutdown: cleans up PID file.
pub fn spawn_daemon(repo_root: &Path, local: bool) -> Result<()> {
    let index_path = db::index_path_for(repo_root, local)?;
    let index_dir = index_path
        .parent()
        .expect("index.db must have a parent directory")
        .to_path_buf();

    // Remove stale PID files from crashed daemons.
    check_stale_pid(&index_dir)?;

    // Enforce single instance.
    if is_running(&index_dir) {
        bail!(
            "daemon is already running for {} (PID file: {})",
            repo_root.display(),
            pid_file_path(&index_dir).display()
        );
    }

    // Daemonize: after this call, we are the grandchild process.
    daemonize()?;

    // Write PID file (we are now the daemon process).
    write_pid(&index_dir)?;

    // Open the database so we can write status.
    let conn = db::open(&index_path)?;

    // Write startup status to daemon_status table.
    write_startup_status(&conn, process::id())?;

    // Register signal handler for graceful shutdown.
    let shutdown = register_signal_handler()?;

    // --- Embedding worker thread ---
    // Create a channel for sending changed file lists to the embedding worker.
    let (embed_tx, embed_rx) = crossbeam_channel::unbounded::<Vec<String>>();
    let embed_shutdown = Arc::clone(&shutdown);
    let embed_index_path = index_path.clone();
    let embed_repo_root = repo_root.to_path_buf();

    let embed_handle = thread::Builder::new()
        .name("wonk-embed".to_string())
        .spawn(move || {
            // Open a dedicated DB connection for the embedding worker.
            let embed_conn = match db::open(&embed_index_path) {
                Ok(c) => c,
                Err(_) => return,
            };
            let client = OllamaClient::new();

            loop {
                // Wait for a batch of changed files, checking shutdown periodically.
                let files = match embed_rx.recv_timeout(Duration::from_secs(1)) {
                    Ok(batch) => coalesce_file_batches(batch, &embed_rx),
                    Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                        if embed_shutdown.load(Ordering::Relaxed) {
                            break;
                        }
                        continue;
                    }
                    Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
                };

                if files.is_empty() {
                    continue;
                }

                match pipeline::reembed_changed_files(
                    &embed_conn,
                    &embed_repo_root,
                    &files,
                    &client,
                ) {
                    Ok(_embedded) => {
                        update_embedding_activity(&embed_conn, files.len()).ok();
                    }
                    Err(e) => {
                        write_error(&embed_conn, &format!("embedding: {e:#}")).ok();
                    }
                }
            }
        })
        .context("spawning embedding worker thread")?;

    // --- File watcher event loop ---
    // Set up debounced file watching (500ms window) and run the event loop.
    let (_watcher, rx) = FileWatcher::new(repo_root, 500).context("starting file watcher")?;

    let repo_root_buf = repo_root.to_path_buf();
    watcher::run_event_loop(&rx, &shutdown, |events| {
        update_queue_depth(&conn, events.len()).ok();

        match pipeline::process_events(&conn, events, &repo_root_buf) {
            Ok(result) => {
                if result.updated_count > 0 {
                    update_activity(&conn).ok();
                }
                // Send changed files to embedding worker (non-blocking).
                if !result.changed_files.is_empty() {
                    let _ = embed_tx.send(result.changed_files);
                }
                update_queue_depth(&conn, 0).ok();
            }
            Err(e) => {
                write_error(&conn, &format!("{e:#}")).ok();
            }
        }
    });

    // --- Graceful shutdown ---
    // Drop the sender to signal the embedding worker to exit.
    drop(embed_tx);
    // Wait for the embedding worker thread to finish.
    let _ = embed_handle.join();

    clear_status(&conn)?;
    remove_pid(&index_dir)?;

    Ok(())
}

/// Stop a running daemon for the given repository by sending SIGTERM.
pub fn stop_daemon(repo_root: &Path, local: bool) -> Result<()> {
    let index_path = db::index_path_for(repo_root, local)?;
    let index_dir = index_path
        .parent()
        .expect("index.db must have a parent directory")
        .to_path_buf();

    let pid_path = pid_file_path(&index_dir);
    let contents = fs::read_to_string(&pid_path).with_context(|| {
        format!(
            "reading PID file {} (is the daemon running?)",
            pid_path.display()
        )
    })?;

    let pid: u32 = contents
        .trim()
        .parse()
        .context("parsing PID from daemon.pid")?;

    if !process_alive(pid) {
        // Process is already gone; clean up the stale PID file.
        remove_pid(&index_dir)?;
        bail!("daemon was not running (stale PID file removed)");
    }

    // Send SIGTERM.
    // SAFETY: sending SIGTERM is a standard POSIX operation.
    let ret = unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
    if ret != 0 {
        bail!("failed to send SIGTERM to PID {pid}");
    }

    // Wait briefly for the process to exit and clean up its PID file.
    for _ in 0..25 {
        thread::sleep(Duration::from_millis(200));
        if !process_alive(pid) {
            // Clean up PID file in case the daemon didn't manage to.
            let _ = remove_pid(&index_dir);
            return Ok(());
        }
    }

    bail!("daemon (PID {pid}) did not exit within 5 seconds after SIGTERM");
}

/// Check the status of the daemon for the given repository.
pub fn daemon_status(repo_root: &Path, local: bool) -> Result<Option<u32>> {
    let index_path = db::index_path_for(repo_root, local)?;
    let index_dir = index_path
        .parent()
        .expect("index.db must have a parent directory")
        .to_path_buf();

    let pid_path = pid_file_path(&index_dir);
    match fs::read_to_string(&pid_path) {
        Ok(contents) => {
            let pid: u32 = contents
                .trim()
                .parse()
                .context("parsing PID from daemon.pid")?;
            if process_alive(pid) {
                Ok(Some(pid))
            } else {
                // Stale PID file.
                remove_pid(&index_dir)?;
                Ok(None)
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).context("reading daemon.pid"),
    }
}

// ---------------------------------------------------------------------------
// Daemon discovery
// ---------------------------------------------------------------------------

/// A discovered daemon entry with its metadata.
#[derive(Debug, Clone, serde::Serialize)]
pub struct DaemonEntry {
    pub pid: u32,
    pub repo_path: String,
    pub uptime: String,
    pub alive: bool,
    #[serde(skip)]
    pub index_dir: PathBuf,
}

/// Format a duration since `start_epoch` (Unix seconds) as a human-readable
/// uptime string.  Returns `"unknown"` if `start_epoch` is `None`.
///
/// Examples: `"3m 12s"`, `"2h 45m"`, `"1d 3h"`, `"unknown"`.
pub fn format_uptime(start_epoch: Option<i64>) -> String {
    let Some(start) = start_epoch else {
        return "unknown".to_string();
    };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    format_duration(now.saturating_sub(start))
}

/// Pure function: format a number of seconds as a human-readable duration.
fn format_duration(secs: i64) -> String {
    if secs < 0 {
        return "unknown".to_string();
    }
    let secs = secs as u64;
    let days = secs / 86400;
    let hours = (secs % 86400) / 3600;
    let minutes = (secs % 3600) / 60;
    let seconds = secs % 60;
    if days > 0 {
        format!("{days}d {hours}h")
    } else if hours > 0 {
        format!("{hours}h {minutes}m")
    } else if minutes > 0 {
        format!("{minutes}m {seconds}s")
    } else {
        format!("{seconds}s")
    }
}

/// Discover daemons under a given `repos_dir` (e.g. `~/.wonk/repos/`)
/// and optionally a local repo root (for `.wonk/daemon.pid`).
///
/// Entries with dead processes have their PID files removed (stale cleanup).
/// Only alive entries are returned.
pub fn discover_daemons_in(repos_dir: &Path, local_repo_root: Option<&Path>) -> Vec<DaemonEntry> {
    let mut entries = Vec::new();
    let mut seen_pids = std::collections::HashSet::new();

    // Scan central repos directory: ~/.wonk/repos/*/daemon.pid
    if repos_dir.is_dir()
        && let Ok(read_dir) = fs::read_dir(repos_dir)
    {
        for entry in read_dir.flatten() {
            let index_dir = entry.path();
            if !index_dir.is_dir() {
                continue;
            }
            if let Some(daemon_entry) = probe_index_dir(&index_dir)
                && seen_pids.insert(daemon_entry.pid)
            {
                entries.push(daemon_entry);
            }
        }
    }

    // Check local index: <repo>/.wonk/daemon.pid
    if let Some(repo_root) = local_repo_root {
        let local_index_dir = repo_root.join(".wonk");
        if local_index_dir.is_dir()
            && let Some(daemon_entry) = probe_index_dir(&local_index_dir)
            && seen_pids.insert(daemon_entry.pid)
        {
            entries.push(daemon_entry);
        }
    }

    entries
}

/// Probe a single index directory for a daemon.pid file.
///
/// Returns `Some(DaemonEntry)` if the PID file exists and the process is alive.
/// Removes stale PID files and returns `None` if the process is dead.
fn probe_index_dir(index_dir: &Path) -> Option<DaemonEntry> {
    let pid_path = pid_file_path(index_dir);
    let contents = fs::read_to_string(&pid_path).ok()?;
    let pid: u32 = contents.trim().parse().ok()?;

    if !process_alive(pid) {
        // Stale: remove the PID file.
        let _ = fs::remove_file(&pid_path);
        return None;
    }

    // Read meta.json for repo path.
    let db_path = index_dir.join("index.db");
    let repo_path = crate::db::read_meta(&db_path)
        .map(|m| m.repo_path)
        .unwrap_or_else(|_| "<unknown>".to_string());

    // Read uptime_start from daemon_status table if possible.
    let uptime = crate::db::open(&db_path)
        .ok()
        .and_then(|conn| read_status(&conn, "uptime_start").ok().flatten())
        .and_then(|s| s.parse::<i64>().ok());

    Some(DaemonEntry {
        pid,
        repo_path,
        uptime: format_uptime(uptime),
        alive: true,
        index_dir: index_dir.to_path_buf(),
    })
}

/// Discover all running daemons (central + local).
///
/// `local_repo_root` is the current repo root (if known), used to also check
/// for a local-mode daemon at `<repo>/.wonk/daemon.pid`.
pub fn discover_all_daemons(local_repo_root: Option<&Path>) -> Vec<DaemonEntry> {
    let repos_dir = std::env::var("HOME")
        .map(|h| PathBuf::from(h).join(".wonk").join("repos"))
        .unwrap_or_default();
    discover_daemons_in(&repos_dir, local_repo_root)
}

/// Stop all running daemons.  Returns a list of `(repo_path, result)` pairs.
pub fn stop_all_daemons(
    local_repo_root: Option<&Path>,
) -> Vec<(String, std::result::Result<(), String>)> {
    let daemons = discover_all_daemons(local_repo_root);
    let mut results = Vec::new();

    for entry in daemons {
        let result = stop_daemon_by_pid(entry.pid, &entry.index_dir);
        results.push((entry.repo_path, result.map_err(|e| format!("{e:#}"))));
    }

    results
}

/// Send SIGTERM to a daemon by PID and wait for it to exit.
fn stop_daemon_by_pid(pid: u32, index_dir: &Path) -> Result<()> {
    if !process_alive(pid) {
        let _ = remove_pid(index_dir);
        bail!("daemon was not running (stale PID file removed)");
    }

    // Send SIGTERM.
    let ret = unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
    if ret != 0 {
        bail!("failed to send SIGTERM to PID {pid}");
    }

    // Wait briefly for exit.
    for _ in 0..25 {
        thread::sleep(Duration::from_millis(200));
        if !process_alive(pid) {
            let _ = remove_pid(index_dir);
            return Ok(());
        }
    }

    bail!("daemon (PID {pid}) did not exit within 5 seconds after SIGTERM");
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;
    use tempfile::TempDir;

    #[test]
    fn test_pid_file_path() {
        let dir = Path::new("/tmp/wonk/test");
        let path = pid_file_path(dir);
        assert_eq!(path, PathBuf::from("/tmp/wonk/test/daemon.pid"));
    }

    #[test]
    fn test_write_and_read_pid() {
        let dir = TempDir::new().unwrap();
        write_pid(dir.path()).unwrap();

        let contents = fs::read_to_string(pid_file_path(dir.path())).unwrap();
        let pid: u32 = contents.trim().parse().unwrap();
        assert_eq!(pid, process::id());
    }

    #[test]
    fn test_remove_pid() {
        let dir = TempDir::new().unwrap();
        write_pid(dir.path()).unwrap();
        assert!(pid_file_path(dir.path()).exists());

        remove_pid(dir.path()).unwrap();
        assert!(!pid_file_path(dir.path()).exists());
    }

    #[test]
    fn test_remove_pid_nonexistent_is_ok() {
        let dir = TempDir::new().unwrap();
        // Should not error when the file doesn't exist.
        remove_pid(dir.path()).unwrap();
    }

    #[test]
    fn test_is_running_no_pid_file() {
        let dir = TempDir::new().unwrap();
        assert!(!is_running(dir.path()));
    }

    #[test]
    fn test_is_running_with_current_process() {
        let dir = TempDir::new().unwrap();
        write_pid(dir.path()).unwrap();
        // Our own process is alive, so is_running should return true.
        assert!(is_running(dir.path()));
    }

    #[test]
    fn test_is_running_with_dead_pid() {
        let dir = TempDir::new().unwrap();
        // Write a PID that almost certainly doesn't exist.
        // PID 4294967 is high enough to be very unlikely to be in use.
        let pid_path = pid_file_path(dir.path());
        fs::write(&pid_path, "4294967\n").unwrap();
        assert!(!is_running(dir.path()));
    }

    #[test]
    fn test_is_running_with_invalid_pid() {
        let dir = TempDir::new().unwrap();
        let pid_path = pid_file_path(dir.path());
        fs::write(&pid_path, "not_a_number\n").unwrap();
        assert!(!is_running(dir.path()));
    }

    #[test]
    fn test_check_stale_pid_removes_dead() {
        let dir = TempDir::new().unwrap();
        let pid_path = pid_file_path(dir.path());
        // Write a dead PID.
        fs::write(&pid_path, "4294967\n").unwrap();
        assert!(pid_path.exists());

        check_stale_pid(dir.path()).unwrap();
        // Should have been removed.
        assert!(!pid_path.exists());
    }

    #[test]
    fn test_check_stale_pid_keeps_alive() {
        let dir = TempDir::new().unwrap();
        // Write our own PID (alive).
        write_pid(dir.path()).unwrap();
        let pid_path = pid_file_path(dir.path());
        assert!(pid_path.exists());

        check_stale_pid(dir.path()).unwrap();
        // Should still be there.
        assert!(pid_path.exists());
    }

    #[test]
    fn test_check_stale_pid_no_file() {
        let dir = TempDir::new().unwrap();
        // Should not error when there's no PID file.
        check_stale_pid(dir.path()).unwrap();
    }

    #[test]
    fn test_process_alive_current() {
        assert!(process_alive(process::id()));
    }

    #[test]
    fn test_process_alive_dead() {
        // PID 4294967 is very unlikely to be alive.
        assert!(!process_alive(4294967));
    }

    #[test]
    fn test_register_signal_handler() {
        let flag = register_signal_handler().unwrap();
        // Flag should initially be false.
        assert!(!flag.load(Ordering::Relaxed));
    }

    #[test]
    fn test_single_instance_enforcement() {
        let dir = TempDir::new().unwrap();
        // Write our own PID to simulate a running daemon.
        write_pid(dir.path()).unwrap();

        // is_running should detect it.
        assert!(is_running(dir.path()));

        // Clean up.
        remove_pid(dir.path()).unwrap();
        assert!(!is_running(dir.path()));
    }

    #[test]
    fn test_write_pid_creates_directory() {
        let dir = TempDir::new().unwrap();
        let nested = dir.path().join("deeply").join("nested").join("dir");
        // Directory doesn't exist yet.
        assert!(!nested.exists());

        write_pid(&nested).unwrap();
        // Directory should now exist.
        assert!(nested.exists());
        // PID file should exist.
        assert!(pid_file_path(&nested).exists());
    }

    #[test]
    fn test_pid_file_content_format() {
        let dir = TempDir::new().unwrap();
        write_pid(dir.path()).unwrap();

        let contents = fs::read_to_string(pid_file_path(dir.path())).unwrap();
        // Should be a number followed by a newline.
        assert!(contents.ends_with('\n'));
        let trimmed = contents.trim();
        assert!(trimmed.parse::<u32>().is_ok());
    }

    // -- Daemon status table tests ------------------------------------------

    fn open_test_db() -> Connection {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("index.db");
        let conn = db::open(&db_path).unwrap();
        // Keep the dir alive by leaking it (tests are short-lived).
        std::mem::forget(dir);
        conn
    }

    #[test]
    fn test_write_and_read_status() {
        let conn = open_test_db();
        write_status(&conn, "foo", "bar").unwrap();
        let val = read_status(&conn, "foo").unwrap();
        assert_eq!(val, Some("bar".to_string()));
    }

    #[test]
    fn test_read_status_missing_key() {
        let conn = open_test_db();
        let val = read_status(&conn, "nonexistent").unwrap();
        assert_eq!(val, None);
    }

    #[test]
    fn test_write_status_upsert() {
        let conn = open_test_db();
        write_status(&conn, "key", "first").unwrap();
        write_status(&conn, "key", "second").unwrap();
        let val = read_status(&conn, "key").unwrap();
        assert_eq!(val, Some("second".to_string()));
    }

    #[test]
    fn test_write_startup_status() {
        let conn = open_test_db();
        let pid = 12345_u32;
        write_startup_status(&conn, pid).unwrap();

        assert_eq!(
            read_status(&conn, "pid").unwrap(),
            Some("12345".to_string())
        );
        assert_eq!(
            read_status(&conn, "state").unwrap(),
            Some("running".to_string())
        );

        let uptime = read_status(&conn, "uptime_start").unwrap().unwrap();
        let ts: i64 = uptime.parse().unwrap();
        assert!(ts > 0, "uptime_start should be a positive timestamp");

        let hb = read_status(&conn, "heartbeat").unwrap().unwrap();
        let hb_ts: i64 = hb.parse().unwrap();
        assert!(hb_ts > 0, "heartbeat should be set on startup");
    }

    #[test]
    fn test_update_activity() {
        let conn = open_test_db();
        update_activity(&conn).unwrap();
        let val = read_status(&conn, "last_activity").unwrap().unwrap();
        let ts: i64 = val.parse().unwrap();
        assert!(ts > 0);
    }

    #[test]
    fn test_update_queue_depth() {
        let conn = open_test_db();
        update_queue_depth(&conn, 42).unwrap();
        assert_eq!(
            read_status(&conn, "files_queued").unwrap(),
            Some("42".to_string())
        );
    }

    #[test]
    fn test_update_queue_depth_zero() {
        let conn = open_test_db();
        update_queue_depth(&conn, 0).unwrap();
        assert_eq!(
            read_status(&conn, "files_queued").unwrap(),
            Some("0".to_string())
        );
    }

    #[test]
    fn test_write_error() {
        let conn = open_test_db();
        write_error(&conn, "something broke").unwrap();
        assert_eq!(
            read_status(&conn, "last_error").unwrap(),
            Some("something broke".to_string())
        );
    }

    #[test]
    fn test_write_heartbeat() {
        let conn = open_test_db();
        write_heartbeat(&conn).unwrap();
        let val = read_status(&conn, "heartbeat").unwrap().unwrap();
        let ts: i64 = val.parse().unwrap();
        assert!(ts > 0);
    }

    #[test]
    fn test_clear_status() {
        let conn = open_test_db();
        write_status(&conn, "a", "1").unwrap();
        write_status(&conn, "b", "2").unwrap();
        write_status(&conn, "c", "3").unwrap();

        clear_status(&conn).unwrap();

        assert_eq!(read_status(&conn, "a").unwrap(), None);
        assert_eq!(read_status(&conn, "b").unwrap(), None);
        assert_eq!(read_status(&conn, "c").unwrap(), None);
    }

    #[test]
    fn test_clear_status_empty_table_ok() {
        let conn = open_test_db();
        // Should not error on an already-empty table.
        clear_status(&conn).unwrap();
    }

    #[test]
    fn test_read_all_status_empty() {
        let conn = open_test_db();
        let info = read_all_status(&conn).unwrap();
        assert!(info.pid.is_none());
        assert!(info.state.is_none());
        assert!(info.uptime_start.is_none());
        assert!(info.last_activity.is_none());
        assert!(info.files_queued.is_none());
        assert!(info.last_error.is_none());
        assert!(info.heartbeat.is_none());
        assert!(info.embedding_last_activity.is_none());
        assert!(info.embedding_files_count.is_none());
    }

    #[test]
    fn test_read_all_status_populated() {
        let conn = open_test_db();
        write_startup_status(&conn, 9999).unwrap();
        update_activity(&conn).unwrap();
        update_queue_depth(&conn, 7).unwrap();
        write_error(&conn, "disk full").unwrap();

        let info = read_all_status(&conn).unwrap();
        assert_eq!(info.pid, Some("9999".to_string()));
        assert_eq!(info.state, Some("running".to_string()));
        assert!(info.uptime_start.is_some());
        assert!(info.last_activity.is_some());
        assert_eq!(info.files_queued, Some("7".to_string()));
        assert_eq!(info.last_error, Some("disk full".to_string()));
        assert!(info.heartbeat.is_some());
    }

    #[test]
    fn test_startup_then_clear_lifecycle() {
        let conn = open_test_db();
        // Startup writes status.
        write_startup_status(&conn, 5555).unwrap();
        update_activity(&conn).unwrap();
        update_queue_depth(&conn, 3).unwrap();

        // Verify we can read it all.
        let info = read_all_status(&conn).unwrap();
        assert_eq!(info.pid, Some("5555".to_string()));
        assert_eq!(info.state, Some("running".to_string()));
        assert_eq!(info.files_queued, Some("3".to_string()));

        // Shutdown clears everything.
        clear_status(&conn).unwrap();
        let info = read_all_status(&conn).unwrap();
        assert!(info.pid.is_none());
        assert!(info.state.is_none());
    }

    #[test]
    fn test_write_status_updates_timestamp() {
        let conn = open_test_db();
        write_status(&conn, "test_key", "val1").unwrap();

        let ts1: i64 = conn
            .query_row(
                "SELECT updated_at FROM daemon_status WHERE key = 'test_key'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(ts1 > 0);

        // Sleep briefly to ensure the timestamp can change.
        thread::sleep(Duration::from_millis(10));

        write_status(&conn, "test_key", "val2").unwrap();
        let ts2: i64 = conn
            .query_row(
                "SELECT updated_at FROM daemon_status WHERE key = 'test_key'",
                [],
                |row| row.get(0),
            )
            .unwrap();

        // Timestamps have second resolution, so ts2 >= ts1.
        assert!(ts2 >= ts1);
    }

    #[test]
    fn test_daemon_info_default() {
        let info = DaemonInfo::default();
        assert!(info.pid.is_none());
        assert!(info.state.is_none());
        assert!(info.uptime_start.is_none());
        assert!(info.last_activity.is_none());
        assert!(info.files_queued.is_none());
        assert!(info.last_error.is_none());
        assert!(info.heartbeat.is_none());
        assert!(info.embedding_last_activity.is_none());
        assert!(info.embedding_files_count.is_none());
    }

    // -- format_uptime / format_duration tests --------------------------------

    #[test]
    fn test_format_duration_seconds() {
        assert_eq!(format_duration(0), "0s");
        assert_eq!(format_duration(45), "45s");
        assert_eq!(format_duration(59), "59s");
    }

    #[test]
    fn test_format_duration_minutes() {
        assert_eq!(format_duration(60), "1m 0s");
        assert_eq!(format_duration(192), "3m 12s");
        assert_eq!(format_duration(3599), "59m 59s");
    }

    #[test]
    fn test_format_duration_hours() {
        assert_eq!(format_duration(3600), "1h 0m");
        assert_eq!(format_duration(9900), "2h 45m");
        assert_eq!(format_duration(86399), "23h 59m");
    }

    #[test]
    fn test_format_duration_days() {
        assert_eq!(format_duration(86400), "1d 0h");
        assert_eq!(format_duration(97200), "1d 3h");
    }

    #[test]
    fn test_format_duration_negative() {
        assert_eq!(format_duration(-1), "unknown");
    }

    #[test]
    fn test_format_uptime_none() {
        assert_eq!(format_uptime(None), "unknown");
    }

    #[test]
    fn test_format_uptime_recent() {
        // Start time is "now" -> uptime should be 0s.
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let result = format_uptime(Some(now));
        assert_eq!(result, "0s");
    }

    // -- discover_daemons_in tests -------------------------------------------

    #[test]
    fn test_discover_daemons_empty_dir() {
        let dir = TempDir::new().unwrap();
        let repos_dir = dir.path().join("repos");
        fs::create_dir(&repos_dir).unwrap();

        let entries = discover_daemons_in(&repos_dir, None);
        assert!(entries.is_empty());
    }

    #[test]
    fn test_discover_daemons_nonexistent_dir() {
        let dir = TempDir::new().unwrap();
        let repos_dir = dir.path().join("nonexistent");

        let entries = discover_daemons_in(&repos_dir, None);
        assert!(entries.is_empty());
    }

    #[test]
    fn test_discover_daemons_stale_cleaned() {
        let dir = TempDir::new().unwrap();
        let repos_dir = dir.path().join("repos");
        let hash_dir = repos_dir.join("abcdef1234567890");
        fs::create_dir_all(&hash_dir).unwrap();

        // Write a PID file with a dead PID.
        let pid_path = hash_dir.join("daemon.pid");
        fs::write(&pid_path, "4294967\n").unwrap();
        assert!(pid_path.exists());

        let entries = discover_daemons_in(&repos_dir, None);
        assert!(entries.is_empty());
        // Stale PID file should have been cleaned up.
        assert!(!pid_path.exists());
    }

    #[test]
    fn test_discover_daemons_alive_process() {
        let dir = TempDir::new().unwrap();
        let repos_dir = dir.path().join("repos");
        let hash_dir = repos_dir.join("abcdef1234567890");
        fs::create_dir_all(&hash_dir).unwrap();

        // Write our own PID (alive).
        let pid_path = hash_dir.join("daemon.pid");
        fs::write(&pid_path, format!("{}\n", process::id())).unwrap();

        let entries = discover_daemons_in(&repos_dir, None);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].pid, process::id());
        assert!(entries[0].alive);
        // No meta.json -> repo_path should be "<unknown>".
        assert_eq!(entries[0].repo_path, "<unknown>");
    }

    #[test]
    fn test_discover_daemons_with_meta_json() {
        let dir = TempDir::new().unwrap();
        let repos_dir = dir.path().join("repos");
        let hash_dir = repos_dir.join("abcdef1234567890");
        fs::create_dir_all(&hash_dir).unwrap();

        // Write a daemon.pid with our PID.
        fs::write(hash_dir.join("daemon.pid"), format!("{}\n", process::id())).unwrap();

        // Write a meta.json.
        let meta = serde_json::json!({
            "repo_path": "/home/user/my-project",
            "created": 1700000000_u64,
            "languages": ["rust"]
        });
        fs::write(
            hash_dir.join("meta.json"),
            serde_json::to_string_pretty(&meta).unwrap(),
        )
        .unwrap();

        let entries = discover_daemons_in(&repos_dir, None);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].repo_path, "/home/user/my-project");
    }

    #[test]
    fn test_discover_daemons_invalid_pid_content() {
        let dir = TempDir::new().unwrap();
        let repos_dir = dir.path().join("repos");
        let hash_dir = repos_dir.join("abcdef1234567890");
        fs::create_dir_all(&hash_dir).unwrap();

        // Write non-numeric PID.
        fs::write(hash_dir.join("daemon.pid"), "not_a_number\n").unwrap();

        let entries = discover_daemons_in(&repos_dir, None);
        assert!(entries.is_empty());
    }

    #[test]
    fn test_discover_daemons_local_mode() {
        let dir = TempDir::new().unwrap();
        let repos_dir = dir.path().join("repos");
        fs::create_dir(&repos_dir).unwrap();

        // Create a local .wonk dir with a daemon.pid.
        let repo_root = dir.path().join("my-repo");
        let local_wonk = repo_root.join(".wonk");
        fs::create_dir_all(&local_wonk).unwrap();
        fs::write(
            local_wonk.join("daemon.pid"),
            format!("{}\n", process::id()),
        )
        .unwrap();

        let entries = discover_daemons_in(&repos_dir, Some(&repo_root));
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].pid, process::id());
    }

    #[test]
    fn test_discover_daemons_dedup_by_pid() {
        let dir = TempDir::new().unwrap();
        let repos_dir = dir.path().join("repos");
        let hash_dir = repos_dir.join("abcdef1234567890");
        fs::create_dir_all(&hash_dir).unwrap();

        let my_pid = process::id();

        // Write our PID in central.
        fs::write(hash_dir.join("daemon.pid"), format!("{my_pid}\n")).unwrap();

        // Also write our PID in local.
        let repo_root = dir.path().join("my-repo");
        let local_wonk = repo_root.join(".wonk");
        fs::create_dir_all(&local_wonk).unwrap();
        fs::write(local_wonk.join("daemon.pid"), format!("{my_pid}\n")).unwrap();

        let entries = discover_daemons_in(&repos_dir, Some(&repo_root));
        // Should deduplicate by PID.
        assert_eq!(entries.len(), 1);
    }

    #[test]
    fn test_daemon_entry_serializable() {
        let entry = DaemonEntry {
            pid: 1234,
            repo_path: "/some/path".to_string(),
            uptime: "3m 12s".to_string(),
            alive: true,
            index_dir: PathBuf::from("/tmp/wonk/index"),
        };
        let json = serde_json::to_string(&entry).unwrap();
        assert!(json.contains("1234"));
        assert!(json.contains("/some/path"));
    }

    // -- Embedding status fields in DaemonInfo --------------------------------

    #[test]
    fn test_daemon_info_embedding_fields_default() {
        let info = DaemonInfo::default();
        assert!(info.embedding_last_activity.is_none());
        assert!(info.embedding_files_count.is_none());
    }

    #[test]
    fn test_read_all_status_includes_embedding_fields() {
        let conn = open_test_db();
        write_status(&conn, "embedding_last_activity", "1700000000").unwrap();
        write_status(&conn, "embedding_files_count", "5").unwrap();

        let info = read_all_status(&conn).unwrap();
        assert_eq!(info.embedding_last_activity, Some("1700000000".to_string()));
        assert_eq!(info.embedding_files_count, Some("5".to_string()));
    }

    #[test]
    fn test_update_embedding_activity_writes_status() {
        let conn = open_test_db();
        update_embedding_activity(&conn, 7).unwrap();

        let info = read_all_status(&conn).unwrap();
        assert!(
            info.embedding_last_activity.is_some(),
            "should write timestamp"
        );
        let ts: i64 = info.embedding_last_activity.unwrap().parse().unwrap();
        assert!(ts > 0, "timestamp should be positive");
        assert_eq!(info.embedding_files_count, Some("7".to_string()));
    }

    #[test]
    fn test_coalesce_file_lists_deduplicates() {
        let (tx, rx) = crossbeam_channel::unbounded::<Vec<String>>();
        tx.send(vec!["a.rs".to_string(), "b.rs".to_string()])
            .unwrap();
        tx.send(vec!["b.rs".to_string(), "c.rs".to_string()])
            .unwrap();

        // Drain the first message.
        let first = rx.recv().unwrap();
        // Coalesce any pending messages with it.
        let combined = coalesce_file_batches(first, &rx);
        assert_eq!(combined.len(), 3);
        assert!(combined.contains(&"a.rs".to_string()));
        assert!(combined.contains(&"b.rs".to_string()));
        assert!(combined.contains(&"c.rs".to_string()));
    }

    #[test]
    fn test_coalesce_file_lists_single_batch() {
        let (_tx, rx) = crossbeam_channel::unbounded::<Vec<String>>();
        // No pending messages.
        let first = vec!["a.rs".to_string()];
        let combined = coalesce_file_batches(first, &rx);
        assert_eq!(combined, vec!["a.rs".to_string()]);
    }
}
