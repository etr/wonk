//! Bounded git-history mining and the file-churn aggregate (TASK-096).
//!
//! One `git log -n <window>` pass mines a commit-count-bounded window of
//! HEAD history: per-commit detail (`mined_commits` + `commit_files`) plus
//! the age-weighted `file_churn` aggregate the rerank signal reads. Cost
//! scales with the window, never with repository age (PRD-HIST-REQ-002).

use std::collections::HashMap;
use std::path::Path;

use anyhow::Result;
use rusqlite::Connection;

/// One mined commit: its sha, committer timestamp (unix seconds), and the
/// repo-relative paths it touched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MinedCommit {
    pub id: String,
    pub ts: i64,
    pub files: Vec<String>,
}

/// Parse `git log --format=%H%x09%ct --name-only` output.
///
/// A line carrying a TAB whose second field parses as a commit timestamp
/// opens a commit; every other non-empty line is a file path of the
/// current commit. Blank separators are skipped, and a trailing newline is
/// tolerated. The timestamp guard keeps a (pathological) tab-containing
/// file path out of the header position.
pub fn parse_git_log(output: &str) -> Vec<MinedCommit> {
    let mut commits: Vec<MinedCommit> = Vec::new();
    for line in output.lines() {
        if line.trim().is_empty() {
            continue;
        }
        if let Some((id, ts)) = split_commit_header(line) {
            commits.push(MinedCommit {
                id: id.to_string(),
                ts,
                files: Vec::new(),
            });
        } else if let Some(commit) = commits.last_mut() {
            commit.files.push(line.to_string());
        }
    }
    commits
}

/// Split a `%H%x09%ct` header line, or `None` when it is not one (no TAB,
/// an empty sha field, or a second field that is not a timestamp).
fn split_commit_header(line: &str) -> Option<(&str, i64)> {
    let (id, ts) = line.split_once('\t')?;
    if id.trim().is_empty() {
        return None;
    }
    Some((id, ts.trim().parse::<i64>().ok()?))
}

/// The age weight of a commit at `ts` (PRD-HIST-REQ-003): linear
/// `1 - (head - ts) / span` within the mined window's span, so the newest
/// commit weighs 1.0 and the oldest ~0.0. A degenerate span (`<= 0` — a
/// single-commit window) weighs every commit 1.0 (raw counts).
///
/// Relative to the MINED WINDOW's newest commit, not the wall clock, so a
/// dormant repository's scores do not decay between indexes.
pub fn age_weight(ts: i64, head_ts: i64, span: i64) -> f32 {
    if span <= 0 {
        return 1.0;
    }
    (1.0 - (head_ts - ts) as f32 / span as f32).clamp(0.0, 1.0)
}

/// `(head_ts, span)` of the mined window: the newest commit timestamp and
/// its distance to the oldest. An empty window is `(0, 0)`.
pub fn window_bounds(commits: &[MinedCommit]) -> (i64, i64) {
    let mut head_ts = 0;
    let mut tail_ts = 0;
    for (i, commit) in commits.iter().enumerate() {
        if i == 0 || commit.ts > head_ts {
            head_ts = commit.ts;
        }
        if i == 0 || commit.ts < tail_ts {
            tail_ts = commit.ts;
        }
    }
    (head_ts, head_ts.saturating_sub(tail_ts))
}

/// The age-weighted churn aggregate: `score(file) = sum(age_weight(ts))`
/// over the retained commits touching it. Rows are summed newest-first as
/// given, so a fixed row order yields a bitwise-identical aggregate.
pub fn aggregate_churn(rows: &[MinedCommit], head_ts: i64, span: i64) -> HashMap<String, f32> {
    let mut scores: HashMap<String, f32> = HashMap::new();
    for commit in rows {
        let weight = age_weight(commit.ts, head_ts, span);
        for file in &commit.files {
            *scores.entry(file.clone()).or_insert(0.0) += weight;
        }
    }
    scores
}

/// Whether `repo_root` looks like a git work tree (a `.git` entry exists).
pub fn has_git(repo_root: &Path) -> bool {
    repo_root.join(".git").exists()
}

/// Mine the newest `window` commits of HEAD history into the history
/// tables, replacing any previous mine.
pub fn mine_full(_conn: &Connection, _repo_root: &Path, _window: usize) -> Result<()> {
    todo!()
}

/// Refresh the history tables after new commits may have landed
/// (PRD-HIST-REQ-007).
pub fn refresh(_conn: &Connection, _repo_root: &Path, _window: usize) -> Result<RefreshOutcome> {
    todo!()
}

/// What a [`refresh`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshOutcome {
    /// Mining disabled or no `.git` — nothing ran, nothing was spawned.
    Skipped,
    /// HEAD unchanged since the last mine — a probe, no rows touched.
    Unchanged,
    /// New commits were folded in and the aggregate recomputed.
    Refreshed,
    /// Git failed; previous data retained (PRD-HIST-REQ-008).
    Failed,
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- parse_git_log ---------------------------------------------------------

    #[test]
    fn parse_git_log_parses_multi_commit_fixture() {
        let log =
            "aaaaaaaa\t100\nsrc/lib.rs\nsrc/my file.rs\n\nbbbbbbbb\t50\nsrc/lib.rs\nREADME.md\n";
        let commits = parse_git_log(log);
        assert_eq!(commits.len(), 2);
        assert_eq!(
            commits[0],
            MinedCommit {
                id: "aaaaaaaa".to_string(),
                ts: 100,
                files: vec!["src/lib.rs".to_string(), "src/my file.rs".to_string()]
            }
        );
        assert_eq!(
            commits[1],
            MinedCommit {
                id: "bbbbbbbb".to_string(),
                ts: 50,
                files: vec!["src/lib.rs".to_string(), "README.md".to_string()]
            }
        );
    }

    #[test]
    fn parse_git_log_empty_and_blank_only_input() {
        assert!(parse_git_log("").is_empty());
        assert!(parse_git_log("\n\n\n").is_empty());
    }

    #[test]
    fn parse_git_log_tolerates_missing_trailing_newline() {
        let commits = parse_git_log("aaaa\t10\nsrc/lib.rs");
        assert_eq!(commits.len(), 1);
        assert_eq!(commits[0].files, vec!["src/lib.rs".to_string()]);
    }

    #[test]
    fn parse_git_log_drops_lines_before_first_header() {
        let commits = parse_git_log("stray.txt\naaaa\t10\nsrc/lib.rs\n");
        assert_eq!(commits.len(), 1);
        assert_eq!(commits[0].files, vec!["src/lib.rs".to_string()]);
    }

    #[test]
    fn parse_git_log_tab_line_with_non_numeric_second_field_is_a_path() {
        // A tab-containing path must not be mistaken for a commit header.
        let commits = parse_git_log("aaaa\t10\nweird\tname.rs\n");
        assert_eq!(commits.len(), 1);
        assert_eq!(commits[0].files, vec!["weird\tname.rs".to_string()]);
    }

    // -- age_weight --------------------------------------------------------

    #[test]
    fn age_weight_newest_is_one_oldest_is_zero() {
        assert_eq!(age_weight(100, 100, 50), 1.0);
        assert_eq!(age_weight(50, 100, 50), 0.0);
        assert!((age_weight(75, 100, 50) - 0.5).abs() < 1e-6);
        // Strictly inside the window stays strictly inside (0, 1).
        assert!(age_weight(99, 100, 50) < 1.0 && age_weight(99, 100, 50) > 0.9);
    }

    #[test]
    fn age_weight_degenerate_span_is_raw_counts() {
        assert_eq!(age_weight(42, 42, 0), 1.0);
        assert_eq!(age_weight(42, 99, 0), 1.0);
    }

    // -- window_bounds -------------------------------------------------------

    #[test]
    fn window_bounds_empty_single_and_spanned() {
        assert_eq!(window_bounds(&[]), (0, 0));
        let one = vec![MinedCommit {
            id: "a".into(),
            ts: 100,
            files: vec![],
        }];
        assert_eq!(window_bounds(&one), (100, 0));
        let three = vec![
            MinedCommit {
                id: "a".into(),
                ts: 100,
                files: vec![],
            },
            MinedCommit {
                id: "b".into(),
                ts: 75,
                files: vec![],
            },
            MinedCommit {
                id: "c".into(),
                ts: 50,
                files: vec![],
            },
        ];
        assert_eq!(window_bounds(&three), (100, 50));
    }

    // -- aggregate_churn -----------------------------------------------------

    #[test]
    fn aggregate_churn_sums_age_weights_per_file() {
        let rows = vec![
            MinedCommit {
                id: "a".into(),
                ts: 100,
                files: vec!["hot.rs".into()],
            },
            MinedCommit {
                id: "b".into(),
                ts: 75,
                files: vec!["hot.rs".into(), "mid.rs".into()],
            },
            MinedCommit {
                id: "c".into(),
                ts: 50,
                files: vec!["hot.rs".into()],
            },
        ];
        let scores = aggregate_churn(&rows, 100, 50);
        assert_eq!(scores.len(), 2);
        assert!((scores["hot.rs"] - 1.5).abs() < 1e-6, "got {:?}", scores);
        assert!((scores["mid.rs"] - 0.5).abs() < 1e-6);
    }

    #[test]
    fn aggregate_churn_empty_is_empty() {
        assert!(aggregate_churn(&[], 0, 0).is_empty());
    }

    // -- has_git --------------------------------------------------------------

    #[test]
    fn has_git_detects_git_dir_and_plain_dir() {
        let with_git = tempfile::TempDir::new().unwrap();
        std::fs::create_dir(with_git.path().join(".git")).unwrap();
        assert!(has_git(with_git.path()));

        let plain = tempfile::TempDir::new().unwrap();
        assert!(!has_git(plain.path()));
    }
}
