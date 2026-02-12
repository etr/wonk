//! Progress indicators for indexing operations.
//!
//! Provides stderr-based progress feedback during `wonk init`, `wonk update`,
//! and auto-initialization. Supports three modes:
//! - **Silent**: no output (piped, --json, --quiet)
//! - **InPlace**: overwrites a single line with `\r` (smart terminal)
//! - **LineBased**: periodic newline-terminated updates (screen-reader friendly)

use std::sync::atomic::{AtomicUsize, Ordering};

use crate::pipeline::IndexStats;

// ---------------------------------------------------------------------------
// ProgressMode
// ---------------------------------------------------------------------------

/// How progress should be displayed on stderr.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgressMode {
    /// No output at all.
    Silent,
    /// Overwrite a single line using `\r` (normal smart terminal).
    InPlace,
    /// Newline-terminated lines at intervals (TERM=dumb, screen readers).
    LineBased,
}

// ---------------------------------------------------------------------------
// Mode detection
// ---------------------------------------------------------------------------

/// Detect the appropriate progress mode.
///
/// - Returns `Silent` if `suppress` is true (e.g. `--json` or `--quiet`).
/// - Returns `Silent` if stderr is not a TTY.
/// - Returns `LineBased` if `TERM=dumb` or TERM is unset.
/// - Returns `InPlace` otherwise.
pub fn detect_mode(suppress: bool) -> ProgressMode {
    if suppress {
        return ProgressMode::Silent;
    }

    use std::io::IsTerminal;
    if !std::io::stderr().is_terminal() {
        return ProgressMode::Silent;
    }

    match std::env::var("TERM").ok().as_deref() {
        None | Some("dumb") => ProgressMode::LineBased,
        _ => ProgressMode::InPlace,
    }
}

// ---------------------------------------------------------------------------
// Progress
// ---------------------------------------------------------------------------

/// Thread-safe progress tracker for indexing operations.
///
/// Call [`inc`] from rayon worker threads after each file is processed.
/// The struct uses atomic counters so no external synchronization is needed.
pub struct Progress {
    mode: ProgressMode,
    total: AtomicUsize,
    done: AtomicUsize,
    /// Active label shown during progress (e.g. "Indexing").
    active_label: String,
    /// Past-tense label shown on completion (e.g. "Indexed").
    done_label: String,
    /// Tracks the last rendered percentage (for LineBased 10% interval throttling).
    last_line_pct: AtomicUsize,
}

impl Progress {
    /// Create a new progress tracker.
    ///
    /// `active_label` is the verb shown during progress (e.g. "Indexing").
    /// `done_label` is the past tense shown on completion (e.g. "Indexed").
    pub fn new(active_label: &str, done_label: &str, mode: ProgressMode) -> Self {
        Self {
            mode,
            total: AtomicUsize::new(0),
            done: AtomicUsize::new(0),
            active_label: active_label.to_string(),
            done_label: done_label.to_string(),
            last_line_pct: AtomicUsize::new(0),
        }
    }

    /// Convenience: create a silent progress tracker (no output).
    pub fn silent() -> Self {
        Self::new("", "", ProgressMode::Silent)
    }

    /// Set the total file count (called after walker pre-scan).
    pub fn set_total(&self, n: usize) {
        self.total.store(n, Ordering::Relaxed);
    }

    /// Atomically increment the done counter by 1.
    ///
    /// Thread-safe; called from rayon worker threads. Rendering is throttled:
    /// - InPlace: every 50 files
    /// - LineBased: at ~10% intervals
    pub fn inc(&self) {
        let prev = self.done.fetch_add(1, Ordering::Relaxed);
        let current = prev + 1;
        let total = self.total.load(Ordering::Relaxed);

        match self.mode {
            ProgressMode::Silent => {}
            ProgressMode::InPlace => {
                if current == total || current % 50 == 0 {
                    self.render_in_place(current, total);
                }
            }
            ProgressMode::LineBased => {
                if total == 0 {
                    return;
                }
                let pct = (current * 100) / total;
                // Round down to nearest 10
                let bucket = pct / 10 * 10;
                let last = self.last_line_pct.load(Ordering::Relaxed);
                if bucket > last || current == total {
                    // Try to claim this bucket
                    if self
                        .last_line_pct
                        .compare_exchange(last, bucket, Ordering::Relaxed, Ordering::Relaxed)
                        .is_ok()
                        || current == total
                    {
                        self.render_line_based(current, total);
                    }
                }
            }
        }
    }

    /// Print the final completion summary.
    pub fn finish(&self, stats: &IndexStats) {
        match self.mode {
            ProgressMode::Silent => {}
            ProgressMode::InPlace => {
                // Overwrite the progress line with the summary
                let msg = format!(
                    "\r{} {} files ({} symbols, {} references) in {:.1}s",
                    self.done_label,
                    stats.file_count,
                    stats.symbol_count,
                    stats.ref_count,
                    stats.elapsed.as_secs_f64(),
                );
                // Pad with spaces to clear any leftover characters from progress line
                eprint!("{:<80}\n", msg);
            }
            ProgressMode::LineBased => {
                eprintln!(
                    "{} {} files ({} symbols, {} references) in {:.1}s",
                    self.done_label,
                    stats.file_count,
                    stats.symbol_count,
                    stats.ref_count,
                    stats.elapsed.as_secs_f64(),
                );
            }
        }
    }

    // -- internal rendering ---------------------------------------------------

    fn render_in_place(&self, current: usize, total: usize) {
        eprint!("\r{}... [{}/{} files]", self.active_label, current, total);
    }

    fn render_line_based(&self, current: usize, total: usize) {
        eprintln!("{}... [{}/{} files]", self.active_label, current, total);
    }

    /// Get the current done count (for testing).
    #[cfg(test)]
    pub fn done(&self) -> usize {
        self.done.load(Ordering::Relaxed)
    }

    /// Get the current total (for testing).
    #[cfg(test)]
    pub fn total(&self) -> usize {
        self.total.load(Ordering::Relaxed)
    }

    /// Get the mode (for testing).
    #[cfg(test)]
    pub fn mode(&self) -> ProgressMode {
        self.mode
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn test_progress_mode_silent_new() {
        let p = Progress::silent();
        assert_eq!(p.mode(), ProgressMode::Silent);
        assert_eq!(p.done(), 0);
        assert_eq!(p.total(), 0);
    }

    #[test]
    fn test_progress_new_with_label() {
        let p = Progress::new("Indexing", "Indexed", ProgressMode::InPlace);
        assert_eq!(p.mode(), ProgressMode::InPlace);
        assert_eq!(p.done(), 0);
        assert_eq!(p.total(), 0);
    }

    #[test]
    fn test_set_total() {
        let p = Progress::new("Indexing", "Indexed", ProgressMode::Silent);
        p.set_total(1234);
        assert_eq!(p.total(), 1234);
    }

    #[test]
    fn test_inc_increments_done() {
        let p = Progress::new("Indexing", "Indexed", ProgressMode::Silent);
        p.set_total(10);
        p.inc();
        p.inc();
        p.inc();
        assert_eq!(p.done(), 3);
    }

    #[test]
    fn test_inc_thread_safe() {
        use std::sync::Arc;
        let p = Arc::new(Progress::new("Indexing", "Indexed", ProgressMode::Silent));
        p.set_total(1000);

        let mut handles = vec![];
        for _ in 0..10 {
            let p = Arc::clone(&p);
            handles.push(std::thread::spawn(move || {
                for _ in 0..100 {
                    p.inc();
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(p.done(), 1000);
    }

    #[test]
    fn test_detect_mode_suppress_returns_silent() {
        assert_eq!(detect_mode(true), ProgressMode::Silent);
    }

    // Note: detect_mode with suppress=false depends on whether stderr is a TTY
    // and the TERM env var, which we can't reliably control in unit tests.
    // The suppress=true path is the critical one to test.

    #[test]
    fn test_finish_silent_does_not_panic() {
        let p = Progress::silent();
        let stats = IndexStats {
            file_count: 100,
            symbol_count: 500,
            ref_count: 2000,
            elapsed: Duration::from_secs_f64(1.5),
        };
        // Should not panic or produce stdout output
        p.finish(&stats);
    }

    #[test]
    fn test_progress_label_stored() {
        let p = Progress::new("Re-indexing", "Re-indexed", ProgressMode::LineBased);
        assert_eq!(p.mode(), ProgressMode::LineBased);
        // labels are private but we can verify mode was set correctly
    }

    #[test]
    fn test_silent_inc_no_panic() {
        // Even with Silent mode, inc should work without panic
        let p = Progress::silent();
        p.set_total(5);
        for _ in 0..5 {
            p.inc();
        }
        assert_eq!(p.done(), 5);
    }

    #[test]
    fn test_progress_mode_enum_values() {
        // Verify all three modes exist and are distinct
        assert_ne!(ProgressMode::Silent, ProgressMode::InPlace);
        assert_ne!(ProgressMode::Silent, ProgressMode::LineBased);
        assert_ne!(ProgressMode::InPlace, ProgressMode::LineBased);
    }
}
