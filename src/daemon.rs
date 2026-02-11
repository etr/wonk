//! Background daemon for file watching and incremental indexing.
//!
//! Provides daemon spawning via double-fork, PID file management,
//! single-instance enforcement, and graceful shutdown via SIGTERM.

use std::fs;
use std::path::{Path, PathBuf};
use std::process;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use fork::{Fork, fork, setsid};
use signal_hook::flag;

use crate::db;

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

    // Register signal handler for graceful shutdown.
    let shutdown = register_signal_handler()?;

    // --- Event loop placeholder ---
    // The real file-watching loop will be implemented in TASK-018.
    // For now, just sleep until SIGTERM/SIGINT.
    while !shutdown.load(Ordering::Relaxed) {
        thread::sleep(Duration::from_millis(200));
    }

    // --- Graceful shutdown ---
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
}
