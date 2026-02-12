//! Background daemon for file watching and incremental indexing.
//!
//! Provides daemon spawning via double-fork, PID file management,
//! single-instance enforcement, graceful shutdown via SIGTERM, and
//! daemon status reporting via the `daemon_status` SQLite table.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use fork::{Fork, fork, setsid};
use rusqlite::Connection;
use signal_hook::flag;

use crate::db;
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
                fs::remove_file(&pid_path).with_context(|| {
                    format!("removing stale PID file {}", pid_path.display())
                })?;
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
        Err(e) => {
            Err(e).with_context(|| format!("removing PID file {}", pid_path.display()))
        }
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

    // --- File watcher event loop ---
    // Set up debounced file watching (500ms window) and run the event loop.
    let (_watcher, rx) = FileWatcher::new(repo_root, 500)
        .context("starting file watcher")?;

    let repo_root_buf = repo_root.to_path_buf();
    watcher::run_event_loop(&rx, &shutdown, |events| {
        update_queue_depth(&conn, events.len()).ok();

        match pipeline::process_events(&conn, events, &repo_root_buf) {
            Ok(count) => {
                if count > 0 {
                    update_activity(&conn).ok();
                }
                update_queue_depth(&conn, 0).ok();
            }
            Err(e) => {
                write_error(&conn, &format!("{e:#}")).ok();
            }
        }
    });

    // --- Graceful shutdown ---
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
    let contents = fs::read_to_string(&pid_path)
        .with_context(|| format!("reading PID file {} (is the daemon running?)", pid_path.display()))?;

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
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
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

        assert_eq!(read_status(&conn, "pid").unwrap(), Some("12345".to_string()));
        assert_eq!(read_status(&conn, "state").unwrap(), Some("running".to_string()));

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
    }
}
