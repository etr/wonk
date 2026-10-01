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
    let head_ts = commits.iter().map(|c| c.ts).max().unwrap_or(0);
    let tail_ts = commits.iter().map(|c| c.ts).min().unwrap_or(0);
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

/// `git log` invocation shared by the full and incremental mines: the
/// newest `window` commits of `range` (None = HEAD), no renames, one TAB
/// header + `--name-only` paths per commit. `-n` bounds cost by the
/// window regardless of repository age (PRD-HIST-REQ-002).
fn git_log(repo_root: &Path, window: usize, range: Option<&str>) -> Result<String> {
    let n = window.to_string();
    let mut args: Vec<&str> = vec![
        "log",
        "-n",
        &n,
        "--no-renames",
        "--format=%H%x09%ct",
        "--name-only",
    ];
    if let Some(range) = range {
        args.push(range);
    }
    crate::impact::run_git_output(repo_root, &args)
}

/// HEAD's full sha, or None outside a repository / on git failure.
fn current_head(repo_root: &Path) -> Option<String> {
    crate::impact::run_git_output(repo_root, &["rev-parse", "HEAD"])
        .ok()
        .map(|out| out.trim().to_string())
}

/// Mine the newest `window` commits of HEAD history into the history
/// tables, replacing any previous mine.
pub fn mine_full(conn: &Connection, repo_root: &Path, window: usize) -> Result<()> {
    let head = current_head(repo_root);
    let commits = parse_git_log(&git_log(repo_root, window, None)?);

    let tx = conn.unchecked_transaction()?;
    tx.execute_batch(
        "DELETE FROM commit_files;
         DELETE FROM mined_commits;
         DELETE FROM file_churn;
         DELETE FROM history_meta;",
    )?;
    insert_commits(&tx, &commits)?;
    trim_to_window(&tx, window)?;
    recompute_file_churn(&tx)?;
    if let Some(head) = head
        && !head.is_empty()
    {
        set_mined_head(&tx, &head)?;
    }
    tx.commit()?;
    Ok(())
}

/// Refresh the history tables after new commits may have landed
/// (PRD-HIST-REQ-007).
///
/// (1) No `.git` → [`RefreshOutcome::Skipped`], nothing spawned.
/// (2) `HEAD` still the mined head → [`RefreshOutcome::Unchanged`] (a
///     millisecond probe). (3) Otherwise the new commits are folded into
///     the retained detail, trimmed to the window, and the aggregate is
///     RECOMPUTED from that detail — which rescales every retained
///     commit's age weight to the new window bounds exactly, without
///     re-reading git. A history rewrite that invalidates the stored head
///     falls back to ONE full re-mine; any git failure warns and returns
///     [`RefreshOutcome::Failed`] with the previous data retained
///     (PRD-HIST-REQ-008) — never an error.
pub fn refresh(conn: &Connection, repo_root: &Path, window: usize) -> Result<RefreshOutcome> {
    if !has_git(repo_root) {
        return Ok(RefreshOutcome::Skipped);
    }
    match refresh_inner(conn, repo_root, window) {
        Ok(outcome) => Ok(outcome),
        Err(e) => {
            eprintln!("wonk: history refresh failed, keeping previous data: {e:#}");
            Ok(RefreshOutcome::Failed)
        }
    }
}

fn refresh_inner(conn: &Connection, repo_root: &Path, window: usize) -> Result<RefreshOutcome> {
    // The shared tail of every full-re-mine path below.
    let full_remine = |conn: &Connection| -> Result<RefreshOutcome> {
        mine_full(conn, repo_root, window)?;
        Ok(RefreshOutcome::Refreshed(mined_count(conn)))
    };

    let head = current_head(repo_root).unwrap_or_default();
    let Some(mined_head) = get_mined_head(conn) else {
        // Never mined (e.g. a pre-TASK-096 index): mine in full.
        return full_remine(conn);
    };
    if head == mined_head {
        return Ok(RefreshOutcome::Unchanged);
    }

    // Incremental mine of mined_head..HEAD, still -n-capped by the window.
    // A rewritten history (rebased-away mined_head) makes the ranged log
    // fail or the stored head invalid: fall back to ONE full re-mine.
    let log = if crate::impact::validate_git_ref(&mined_head).is_ok() {
        match git_log(repo_root, window, Some(&format!("{mined_head}..HEAD"))) {
            Ok(log) => log,
            Err(e) => {
                eprintln!("wonk: incremental history mine failed, re-mining in full: {e:#}");
                return full_remine(conn);
            }
        }
    } else {
        eprintln!("wonk: stored mined head {mined_head:?} is not a valid ref, re-mining in full");
        return full_remine(conn);
    };
    let commits = parse_git_log(&log);

    let tx = conn.unchecked_transaction()?;
    insert_commits(&tx, &commits)?;
    trim_to_window(&tx, window)?;
    recompute_file_churn(&tx)?;
    set_mined_head(&tx, &head)?;
    tx.commit()?;
    Ok(RefreshOutcome::Refreshed(commits.len()))
}

fn mined_count(conn: &Connection) -> usize {
    conn.query_row("SELECT COUNT(*) FROM mined_commits", [], |row| {
        row.get::<_, i64>(0)
    })
    .unwrap_or(0) as usize
}

/// The sha recorded at the last successful mine, if any.
fn get_mined_head(conn: &Connection) -> Option<String> {
    conn.query_row(
        "SELECT value FROM history_meta WHERE key = 'mined_head'",
        [],
        |row| row.get(0),
    )
    .ok()
}

fn set_mined_head(conn: &Connection, head: &str) -> Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO history_meta(key, value) VALUES ('mined_head', ?1)",
        rusqlite::params![head],
    )?;
    Ok(())
}

/// Insert per-commit detail rows. INSERT OR IGNORE: a re-mined overlap
/// (a commit landed between probe and log) must not fail the refresh.
fn insert_commits(conn: &Connection, commits: &[MinedCommit]) -> Result<()> {
    for commit in commits {
        conn.execute(
            "INSERT OR IGNORE INTO mined_commits(commit_id, commit_ts) VALUES (?1, ?2)",
            rusqlite::params![commit.id, commit.ts],
        )?;
        for file in &commit.files {
            conn.execute(
                "INSERT OR IGNORE INTO commit_files(commit_id, file) VALUES (?1, ?2)",
                rusqlite::params![commit.id, file],
            )?;
        }
    }
    Ok(())
}

/// Retain only the newest `window` commits, newest timestamp first with a
/// commit-id tie-break so the boundary is deterministic; both detail
/// tables are trimmed explicitly so correctness never depends on the
/// foreign_keys pragma being enabled.
fn trim_to_window(conn: &Connection, window: usize) -> Result<()> {
    let window = window as i64;
    conn.execute(
        "DELETE FROM commit_files WHERE commit_id NOT IN \
         (SELECT commit_id FROM mined_commits ORDER BY commit_ts DESC, commit_id DESC LIMIT ?1)",
        rusqlite::params![window],
    )?;
    conn.execute(
        "DELETE FROM mined_commits WHERE commit_id NOT IN \
         (SELECT commit_id FROM mined_commits ORDER BY commit_ts DESC, commit_id DESC LIMIT ?1)",
        rusqlite::params![window],
    )?;
    Ok(())
}

/// Recompute `file_churn` from the retained detail — the one aggregate
/// implementation. Rows are read newest-first (`commit_ts DESC,
/// commit_id DESC`) so the sum order — and therefore the stored scores —
/// is deterministic regardless of git's log order. Bounds come from ALL
/// retained commits (a file-less commit still bounds the window), and
/// weights are derived from the RETAINED window's own bounds, so folding
/// in new commits rescales every retained commit's weight exactly.
fn recompute_file_churn(conn: &Connection) -> Result<()> {
    let mut commits: Vec<MinedCommit> = {
        let mut stmt = conn.prepare(
            "SELECT commit_id, commit_ts FROM mined_commits \
             ORDER BY commit_ts DESC, commit_id DESC",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(MinedCommit {
                id: row.get(0)?,
                ts: row.get(1)?,
                files: Vec::new(),
            })
        })?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    let index_by_id: HashMap<String, usize> = commits
        .iter()
        .enumerate()
        .map(|(i, commit)| (commit.id.clone(), i))
        .collect();
    {
        let mut stmt = conn.prepare("SELECT commit_id, file FROM commit_files")?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        let files: Vec<(String, String)> = rows.collect::<rusqlite::Result<_>>()?;
        for (id, file) in files {
            if let Some(i) = index_by_id.get(&id) {
                commits[*i].files.push(file);
            }
        }
    }

    let (head_ts, span) = window_bounds(&commits);
    let scores = aggregate_churn(&commits, head_ts, span);

    conn.execute("DELETE FROM file_churn", [])?;
    let mut insert = conn.prepare("INSERT INTO file_churn(file, score) VALUES (?1, ?2)")?;
    for (file, score) in &scores {
        insert.execute(rusqlite::params![file, score])?;
    }
    Ok(())
}

/// What a [`refresh`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshOutcome {
    /// Mining disabled or no `.git` — nothing ran, nothing was spawned.
    Skipped,
    /// HEAD unchanged since the last mine — a probe, no rows touched.
    Unchanged,
    /// `n` new commits were folded in and the aggregate recomputed.
    Refreshed(usize),
    /// Git failed; previous data retained (PRD-HIST-REQ-008).
    Failed,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use tempfile::TempDir;

    fn git_available() -> bool {
        Command::new("git")
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success())
    }

    /// An in-memory-index connection over a real git repo with scripted
    /// commits: `(file, ts)` pairs become one dated commit each, oldest
    /// first, so the fixture's HEAD history is fully determined.
    fn make_history_repo(commits: &[(&str, i64)]) -> (TempDir, Connection) {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        for arg in [
            vec!["init"],
            vec!["config", "user.email", "test@test.com"],
            vec!["config", "user.name", "Test"],
        ] {
            let ok = Command::new("git")
                .args(&arg)
                .current_dir(root)
                .output()
                .unwrap();
            assert!(ok.status.success(), "git {:?} failed", arg);
        }
        for (i, (file, ts)) in commits.iter().enumerate() {
            if let Some(parent) = Path::new(file).parent() {
                std::fs::create_dir_all(root.join(parent)).unwrap();
            }
            std::fs::write(root.join(file), format!("content {i}\n")).unwrap();
            let date = format!("@{ts} +0000");
            let ok = Command::new("git")
                .args(["add", "."])
                .current_dir(root)
                .output()
                .unwrap();
            assert!(ok.status.success());
            let ok = Command::new("git")
                .args(["commit", "-m", &format!("c{i}"), "--allow-empty"])
                .env("GIT_AUTHOR_DATE", &date)
                .env("GIT_COMMITTER_DATE", &date)
                .current_dir(root)
                .output()
                .unwrap();
            assert!(ok.status.success(), "commit {i} failed");
        }
        let conn = crate::db::open(&dir.path().join("history.db")).unwrap();
        (dir, conn)
    }

    fn churn_score(conn: &Connection, file: &str) -> Option<f64> {
        conn.query_row(
            "SELECT score FROM file_churn WHERE file = ?1",
            rusqlite::params![file],
            |row| row.get(0),
        )
        .ok()
    }

    fn mined_count(conn: &Connection) -> i64 {
        conn.query_row("SELECT COUNT(*) FROM mined_commits", [], |row| row.get(0))
            .unwrap()
    }

    fn mined_head(conn: &Connection) -> String {
        conn.query_row(
            "SELECT value FROM history_meta WHERE key = 'mined_head'",
            [],
            |row| row.get(0),
        )
        .unwrap()
    }

    #[test]
    fn mine_full_retains_min_of_window_and_total() {
        if !git_available() {
            return;
        }
        let commits: Vec<(&str, i64)> = (0..5).map(|i| ("src/lib.rs", 100 + i)).collect();
        let (dir, conn) = make_history_repo(&commits);

        mine_full(&conn, dir.path(), 3).unwrap();
        assert_eq!(mined_count(&conn), 3, "window smaller than history");

        mine_full(&conn, dir.path(), 10).unwrap();
        assert_eq!(mined_count(&conn), 5, "history smaller than window");
    }

    #[test]
    fn mine_full_weights_recent_changes_higher() {
        if !git_available() {
            return;
        }
        let (dir, conn) = make_history_repo(&[
            ("dormant.rs", 100),
            ("dormant.rs", 200),
            ("hot.rs", 300),
            ("hot.rs", 400),
        ]);
        mine_full(&conn, dir.path(), 4).unwrap();

        let dormant = churn_score(&conn, "dormant.rs").unwrap();
        let hot = churn_score(&conn, "hot.rs").unwrap();
        assert!(
            hot > dormant,
            "hot file must outscore dormant: {hot} vs {dormant}"
        );
        // Exact: dormant = 0 + 1/3; hot = 2/3 + 1.
        assert!((dormant - 1.0 / 3.0).abs() < 1e-6, "got {dormant}");
        assert!((hot - (2.0 / 3.0 + 1.0)).abs() < 1e-6, "got {hot}");
    }

    #[test]
    fn refresh_is_unchanged_when_head_stable() {
        if !git_available() {
            return;
        }
        let (dir, conn) = make_history_repo(&[("src/lib.rs", 100), ("src/lib.rs", 200)]);
        mine_full(&conn, dir.path(), 10).unwrap();
        let before = churn_score(&conn, "src/lib.rs").unwrap();
        let rows = mined_count(&conn);
        let head = mined_head(&conn);

        assert_eq!(
            refresh(&conn, dir.path(), 10).unwrap(),
            RefreshOutcome::Unchanged
        );
        assert_eq!(mined_count(&conn), rows);
        assert_eq!(churn_score(&conn, "src/lib.rs"), Some(before));
        assert_eq!(mined_head(&conn), head);
    }

    #[test]
    fn refresh_folds_new_commit_and_rescales_old_scores() {
        if !git_available() {
            return;
        }
        let (dir, conn) = make_history_repo(&[("old.rs", 100), ("old.rs", 110), ("old.rs", 120)]);
        mine_full(&conn, dir.path(), 3).unwrap();
        // span 20: old.rs = 0 + 0.5 + 1.0 = 1.5.
        assert!((churn_score(&conn, "old.rs").unwrap() - 1.5).abs() < 1e-6);

        let date = "@130 +0000";
        std::fs::write(dir.path().join("new.rs"), "new\n").unwrap();
        Command::new("git")
            .args(["add", "."])
            .current_dir(dir.path())
            .output()
            .unwrap();
        let ok = Command::new("git")
            .args(["commit", "-m", "c3"])
            .env("GIT_AUTHOR_DATE", date)
            .env("GIT_COMMITTER_DATE", date)
            .current_dir(dir.path())
            .output()
            .unwrap();
        assert!(ok.status.success());

        assert_eq!(
            refresh(&conn, dir.path(), 3).unwrap(),
            RefreshOutcome::Refreshed(1)
        );

        // Retained window is now {110, 120, 130}: span 20 again, and the
        // old commits' weights RESCALED — old.rs = 0 + 0.5 = 0.5 (an
        // append-only score would still read 1.5), new.rs = 1.0.
        assert_eq!(mined_count(&conn), 3);
        assert!((churn_score(&conn, "old.rs").unwrap() - 0.5).abs() < 1e-6);
        assert!((churn_score(&conn, "new.rs").unwrap() - 1.0).abs() < 1e-6);
    }

    #[test]
    fn refresh_trims_to_window_when_many_new_commits_land() {
        if !git_available() {
            return;
        }
        let (dir, conn) = make_history_repo(&[("a.rs", 100)]);
        mine_full(&conn, dir.path(), 3).unwrap();

        for (i, ts) in (110..130).enumerate() {
            let file = format!("f{i}.rs");
            std::fs::write(dir.path().join(&file), "x\n").unwrap();
            let date = format!("@{ts} +0000");
            Command::new("git")
                .args(["add", "."])
                .current_dir(dir.path())
                .output()
                .unwrap();
            let ok = Command::new("git")
                .args(["commit", "-m", &format!("n{i}")])
                .env("GIT_AUTHOR_DATE", &date)
                .env("GIT_COMMITTER_DATE", &date)
                .current_dir(dir.path())
                .output()
                .unwrap();
            assert!(ok.status.success());
        }

        let outcome = refresh(&conn, dir.path(), 3).unwrap();
        assert!(matches!(outcome, RefreshOutcome::Refreshed(_)));
        assert_eq!(mined_count(&conn), 3, "retained rows must equal the window");
        // Only the newest three commits' files remain in the aggregate.
        assert!(churn_score(&conn, "a.rs").is_none());
        assert!(churn_score(&conn, "f0.rs").is_none());
        assert!(churn_score(&conn, "f19.rs").is_some());
    }

    #[test]
    fn refresh_reports_failed_and_retains_data_on_corrupt_git() {
        if !git_available() {
            return;
        }
        let (dir, conn) = make_history_repo(&[("src/lib.rs", 100)]);
        mine_full(&conn, dir.path(), 10).unwrap();
        let before = churn_score(&conn, "src/lib.rs").unwrap();

        // Corrupt the repository after a good mine.
        std::fs::write(dir.path().join(".git/HEAD"), "garbage\n").unwrap();
        assert_eq!(
            refresh(&conn, dir.path(), 10).unwrap(),
            RefreshOutcome::Failed
        );
        assert_eq!(churn_score(&conn, "src/lib.rs"), Some(before));
        assert_eq!(mined_count(&conn), 1);
    }

    #[test]
    fn mine_full_window_one_retains_head_only() {
        if !git_available() {
            return;
        }
        let (dir, conn) = make_history_repo(&[("a.rs", 100), ("b.rs", 200), ("c.rs", 300)]);
        mine_full(&conn, dir.path(), 1).unwrap();

        assert_eq!(mined_count(&conn), 1);
        // Degenerate span: the single retained commit weighs 1.0.
        assert!(churn_score(&conn, "a.rs").is_none());
        assert!(churn_score(&conn, "b.rs").is_none());
        assert!((churn_score(&conn, "c.rs").unwrap() - 1.0).abs() < 1e-6);
    }

    #[test]
    fn refresh_without_git_dir_is_skipped_without_spawning() {
        if !git_available() {
            return;
        }
        let dir = TempDir::new().unwrap();
        let conn = crate::db::open(&dir.path().join("index.db")).unwrap();
        assert_eq!(
            refresh(&conn, dir.path(), 10).unwrap(),
            RefreshOutcome::Skipped
        );
        assert_eq!(mined_count(&conn), 0);
    }

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

    // -- acceptance criteria (TASK-096) ----------------------------------------

    /// A candidate result for the rerank seam, same category for every
    /// candidate so the churn signal alone decides the order.
    fn churn_candidate(file: &str) -> crate::ranker::ClassifiedResult {
        crate::ranker::ClassifiedResult {
            result: crate::search::SearchResult {
                file: std::path::PathBuf::from(file),
                line: 1,
                col: 1,
                content: "fn target() {}".to_string(),
            },
            category: crate::ranker::ResultCategory::Definition,
            annotation: None,
        }
    }

    fn churn_only_weights() -> crate::rerank::WeightTable {
        crate::rerank::WeightTable::from_pairs([("churn".to_string(), 1.0)]).unwrap()
    }

    #[test]
    fn ac1_frequently_modified_file_outranks_dormant_one() {
        if !git_available() {
            return;
        }
        // A filler commit anchors the window's oldest edge so dormant.rs
        // sits mid-window with a positive (but lower) weight; hot.rs is
        // touched by the five newest commits.
        let mut commits = vec![
            ("filler.rs", 1_000_000_000i64),
            ("dormant.rs", 1_000_000_050),
        ];
        for k in 0..5 {
            commits.push(("hot.rs", 1_000_000_100 + k));
        }
        let (dir, _seed) = make_history_repo(&commits);
        crate::pipeline::build_index(dir.path(), true).unwrap();
        let conn = crate::db::open_existing(&crate::db::local_index_path(dir.path())).unwrap();

        let scored = crate::rerank::rerank(
            vec![churn_candidate("dormant.rs"), churn_candidate("hot.rs")],
            &crate::rerank::QueryInfo { pattern: "target" },
            Some(&conn),
            &churn_only_weights(),
            &crate::rerank::ContextSources::default(),
        );

        assert_eq!(
            scored[0].classified.result.file,
            std::path::PathBuf::from("hot.rs"),
            "hot file must rank first: {scored:?}"
        );
        let value = |f: &str| {
            scored
                .iter()
                .find(|s| s.classified.result.file == *f)
                .unwrap()
                .contributions[0]
                .value
        };
        assert!(value("hot.rs") > 0.0 && value("dormant.rs") > 0.0);
        assert!(value("hot.rs") > value("dormant.rs"));
    }

    /// A repo with `n` dated commits, one per second, rotating five files
    /// (`i % 5`); a single git spawn per commit keeps big fixtures fast.
    /// The newest `k` commits of `make_dated_repo(n, base)` and
    /// `make_dated_repo(m, base - (m - n))` are IDENTICAL whenever the
    /// offset makes the timestamps line up — the identical-tail pair the
    /// cost-not-age check needs.
    fn make_dated_repo(n: usize, base_ts: i64) -> TempDir {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        for args in [
            vec!["init"],
            vec!["config", "user.email", "test@test.com"],
            vec!["config", "user.name", "Test"],
        ] {
            let ok = Command::new("git")
                .args(&args)
                .current_dir(root)
                .output()
                .unwrap();
            assert!(ok.status.success(), "git {args:?} failed");
        }
        for i in 0..n {
            std::fs::write(root.join(format!("f{}.rs", i % 5)), format!("c{i}\n")).unwrap();
            let date = format!("@{} +0000", base_ts + i as i64);
            let message = format!("c{i}");
            if i == 0 {
                // Track the files once; -a carries every later change so the
                // loop stays at one git spawn per commit.
                let ok = Command::new("git")
                    .args(["add", "."])
                    .current_dir(root)
                    .output()
                    .unwrap();
                assert!(ok.status.success());
            }
            let args: Vec<&str> = vec!["commit", "-m", &message, "--allow-empty", "-a"];
            let ok = Command::new("git")
                .args(&args)
                .env("GIT_AUTHOR_DATE", &date)
                .env("GIT_COMMITTER_DATE", &date)
                .current_dir(root)
                .output()
                .unwrap();
            assert!(ok.status.success(), "commit {i} failed");
        }
        dir
    }

    fn full_churn_map(conn: &Connection) -> Vec<(String, f64)> {
        let mut stmt = conn
            .prepare("SELECT file, score FROM file_churn ORDER BY file")
            .unwrap();
        let rows = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap();
        rows.collect::<rusqlite::Result<_>>().unwrap()
    }

    #[test]
    fn ac2_window_is_the_structural_cost_bound_not_repo_age() {
        if !git_available() {
            return;
        }
        // ~500-commit repository, mined at the default window: the mine
        // completes well inside the index build budget.
        let dir = make_dated_repo(500, 1_000_000_000);
        let conn = crate::db::open(&dir.path().join("ac2.db")).unwrap();
        let start = std::time::Instant::now();
        mine_full(&conn, dir.path(), 500).unwrap();
        let elapsed = start.elapsed();
        assert_eq!(mined_count(&conn), 500, "whole history within the window");
        assert!(
            elapsed.as_secs_f64() < 10.0,
            "window=500 mine exceeded the smoke budget: {elapsed:?}"
        );

        // The structural bound: cost is proportional to the window because
        // git reads at most `window` commits — window=50 retains EXACTLY 50
        // rows of the same 500-commit history.
        mine_full(&conn, dir.path(), 50).unwrap();
        assert_eq!(mined_count(&conn), 50);
        let short_map = full_churn_map(&conn);

        // Cost is not repo age: a LONGER history whose newest 50 commits
        // are identical (same timestamps, same files) retains the identical
        // rows and the identical aggregate.
        let longer = make_dated_repo(550, 999_999_950);
        let conn_long = crate::db::open(&longer.path().join("ac2b.db")).unwrap();
        mine_full(&conn_long, longer.path(), 50).unwrap();
        assert_eq!(mined_count(&conn_long), 50);
        assert_eq!(
            full_churn_map(&conn_long),
            short_map,
            "identical tail at the same window must give the identical aggregate"
        );
    }

    #[test]
    fn ac4_window_size_is_configurable_and_changes_the_result() {
        if !git_available() {
            return;
        }
        let dir = make_dated_repo(60, 1_000_000_000);
        let conn = crate::db::open(&dir.path().join("ac4.db")).unwrap();

        mine_full(&conn, dir.path(), 3).unwrap();
        assert_eq!(mined_count(&conn), 3);
        let tight = full_churn_map(&conn);

        mine_full(&conn, dir.path(), 50).unwrap();
        assert_eq!(mined_count(&conn), 50);
        let wide = full_churn_map(&conn);

        assert_ne!(
            tight, wide,
            "the window size must change the mined aggregate"
        );
    }
}
