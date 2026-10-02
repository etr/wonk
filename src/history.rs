//! Bounded git-history mining and its rerank aggregates (TASK-096/097).
//!
//! One `git log -n <window>` pass mines a commit-count-bounded window of
//! HEAD history: per-commit detail (`mined_commits` + `commit_files`) plus
//! the age-weighted `file_churn` aggregate the churn signal reads and the
//! top-K-per-file `co_change` coupling the co-change signal reads. Cost
//! scales with the window, never with repository age (PRD-HIST-REQ-002).

use std::collections::HashMap;
use std::path::Path;

use anyhow::Result;
use rusqlite::Connection;

/// One mined commit: its sha, committer timestamp (unix seconds), author
/// name (TASK-105; `None` when the log line carried none — the 2-field
/// legacy header — or an empty `%an`), and the repo-relative paths it
/// touched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MinedCommit {
    pub id: String,
    pub ts: i64,
    pub author: Option<String>,
    pub files: Vec<String>,
}

/// Parse `git log --format=%H%x09%ct%x09%an --name-only` output.
///
/// A line carrying a TAB whose second field parses as a commit timestamp
/// opens a commit; every other non-empty line is a file path of the
/// current commit. Blank separators are skipped, and a trailing newline is
/// tolerated. The timestamp guard keeps a (pathological) tab-containing
/// file path out of the header position. The author is the third TAB
/// field; the 2-field legacy header (old stored detail) parses with
/// `author: None`. Path lines are stored under
/// their REAL names: a C-style-quoted line (git quotes any path with a
/// quote, backslash, or control byte even under `core.quotePath=false`)
/// is unquoted by [`unquote_git_path`] first.
pub fn parse_git_log(output: &str) -> Vec<MinedCommit> {
    let mut commits: Vec<MinedCommit> = Vec::new();
    for line in output.lines() {
        if line.trim().is_empty() {
            continue;
        }
        if let Some((id, ts, author)) = split_commit_header(line) {
            commits.push(MinedCommit {
                id: id.to_string(),
                ts,
                author: author.map(|a| a.to_string()),
                files: Vec::new(),
            });
        } else if let Some(commit) = commits.last_mut() {
            commit.files.push(unquote_git_path(line));
        }
    }
    commits
}

/// Decode one C-style-quoted git path line to the real path.
///
/// A quoted pair (`"..."`) has its escape sequences decoded: `\t` `\n`
/// `\\` `\"` map directly, and a `\NNN` octal triple is one raw byte
/// (git emits a UTF-8 sequence one byte per triple — `"caf\303\251"` is
/// `café`); the bytes are then read as (possibly lossy) UTF-8. Anything
/// else — an unquoted line (the `core.quotePath=false` form), an
/// unrecognized escape, or a dangling backslash — is kept verbatim: the
/// parser never guesses a name it was not given.
fn unquote_git_path(line: &str) -> String {
    let Some(inner) = line
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
    else {
        return line.to_string();
    };
    let bytes = inner.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let byte = bytes[i];
        if byte != b'\\' {
            out.push(byte);
            i += 1;
            continue;
        }
        let Some(&next) = bytes.get(i + 1) else {
            out.push(byte); // dangling backslash: keep it
            i += 1;
            continue;
        };
        match next {
            b't' => {
                out.push(b'\t');
                i += 2;
            }
            b'n' => {
                out.push(b'\n');
                i += 2;
            }
            b'\\' => {
                out.push(b'\\');
                i += 2;
            }
            b'"' => {
                out.push(b'"');
                i += 2;
            }
            b'0'..=b'7' => {
                let digits = bytes.get(i + 1..i + 4).unwrap_or(&[]);
                let octal = std::str::from_utf8(digits)
                    .ok()
                    .and_then(|s| u8::from_str_radix(s, 8).ok())
                    .filter(|_| {
                        digits.len() == 3 && digits.iter().all(|d| (b'0'..=b'7').contains(d))
                    });
                match octal {
                    Some(value) => {
                        out.push(value);
                        i += 4;
                    }
                    None => {
                        out.push(byte); // not a full \NNN triple: keep verbatim
                        i += 1;
                    }
                }
            }
            _ => {
                out.push(byte); // unknown escape: keep the backslash
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Split a `%H%x09%ct(%x09%an)` header line into `(sha, ts, author)`, or
/// `None` when it is not one (no TAB, an empty sha field, or a second
/// field that is not a timestamp). The third field is the author name
/// (`Some("")` from the log maps to `None` at the caller); a 2-field
/// legacy header yields `author: None`.
fn split_commit_header(line: &str) -> Option<(&str, i64, Option<&str>)> {
    let (id, rest) = line.split_once('\t')?;
    if id.trim().is_empty() {
        return None;
    }
    match rest.split_once('\t') {
        Some((ts, author)) => {
            let ts = ts.trim().parse::<i64>().ok()?;
            let author = if author.is_empty() { None } else { Some(author) };
            Some((id, ts, author))
        }
        None => Some((id, rest.trim().parse::<i64>().ok()?, None)),
    }
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

/// The per-file author/recency facts the feedback features read
/// (TASK-105, PRD-FB-REQ-023/028): the newest commit's timestamp and
/// author, and the file's dominant author.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FileHistory {
    /// Newest commit ts touching the file (unix seconds).
    pub last_ts: i64,
    /// Author of that newest commit; `None` when the detail row predates
    /// author capture or git recorded an empty name.
    pub last_author: Option<String>,
    /// Argmax of age-weighted per-author commit count; `None` when no
    /// retained commit touching the file carries an author.
    pub primary_author: Option<String>,
}

/// Fold the per-file history facts (TASK-105) from the retained commits,
/// newest-first as [`recompute_history_aggregates`] reads them: the FIRST
/// sighting of a file fixes `last_ts`/`last_author`, and `primary_author`
/// is the argmax of age-weighted per-author commit count, ties breaking
/// to the lexicographically smallest author name — a total order, so
/// HashMap iteration order cannot leak into the stored aggregate.
pub fn aggregate_file_history(
    rows: &[MinedCommit],
    head_ts: i64,
    span: i64,
) -> HashMap<String, FileHistory> {
    let mut history: HashMap<String, FileHistory> = HashMap::new();
    let mut weighted: HashMap<(String, String), f32> = HashMap::new();
    for commit in rows {
        let weight = age_weight(commit.ts, head_ts, span);
        for file in &commit.files {
            // Rows are newest-first: the first sighting IS the newest.
            history.entry(file.clone()).or_insert_with(|| FileHistory {
                last_ts: commit.ts,
                last_author: commit.author.clone(),
                primary_author: None,
            });
            if let Some(author) = &commit.author {
                *weighted.entry((file.clone(), author.clone())).or_insert(0.0) += weight;
            }
        }
    }
    let mut by_file: HashMap<&str, Vec<(&str, f32)>> = HashMap::new();
    for ((file, author), weight) in &weighted {
        by_file
            .entry(file.as_str())
            .or_default()
            .push((author.as_str(), *weight));
    }
    for (file, mut authors) in by_file {
        // argmax of weighted count, ties to the lexicographically smallest
        // name — (count desc, name asc) is a total order.
        authors.sort_by(|(name_a, count_a), (name_b, count_b)| {
            count_b
                .partial_cmp(count_a)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| name_a.cmp(name_b))
        });
        if let Some((author, _)) = authors.first() {
            history.entry(file.to_string()).or_default().primary_author = Some(author.to_string());
        }
    }
    history
}

/// One retained co-change coupling: `file_a`'s directed coupling to
/// `file_b` (TASK-097). Both directions of a pair are stored
/// independently — each carries its own top-K retention.
#[derive(Debug, Clone, PartialEq)]
pub struct CoChangeRow {
    pub file_a: String,
    pub file_b: String,
    pub weight: f32,
}

/// The age-weighted co-occurrence aggregate (TASK-097, PRD-HIST-REQ-004/005):
/// `weight(a, b) = sum(age_weight(ts))` over the retained commits touching
/// BOTH `a` and `b`, using the same [`age_weight`] machinery as churn — so
/// folding in new commits rescales every retained pair exactly.
///
/// A commit touching strictly more than `max_commit_files` files is EXCLUDED
/// from co-change derivation only (PRD-HIST-REQ-005): reformatting sweeps
/// and vendored imports would otherwise couple everything to everything.
/// It still feeds churn and still bounds the window. Rows are summed
/// newest-first as given; every ordered pair `a != b` is accumulated, which
/// makes the aggregate symmetric by construction.
pub fn aggregate_co_change(
    rows: &[MinedCommit],
    head_ts: i64,
    span: i64,
    max_commit_files: usize,
) -> HashMap<String, HashMap<String, f32>> {
    let mut weights: HashMap<String, HashMap<String, f32>> = HashMap::new();
    for commit in rows {
        if commit.files.len() > max_commit_files {
            continue;
        }
        let weight = age_weight(commit.ts, head_ts, span);
        for file_a in &commit.files {
            for file_b in &commit.files {
                if file_a == file_b {
                    continue;
                }
                *weights
                    .entry(file_a.clone())
                    .or_default()
                    .entry(file_b.clone())
                    .or_insert(0.0) += weight;
            }
        }
    }
    weights
}

/// Retain each `file_a`'s `top_k` strongest partners (weight DESC,
/// `file_b` ASC tie-break), each direction independently — so storage is
/// linear in files, never quadratic (PRD-HIST-REQ-004). The result is
/// sorted by `(file_a, file_b)`, the canonical insert order.
pub fn top_k_per_file(
    weights: &HashMap<String, HashMap<String, f32>>,
    top_k: usize,
) -> Vec<CoChangeRow> {
    let mut rows: Vec<CoChangeRow> = Vec::new();
    for (file_a, partners) in weights {
        let mut ranked: Vec<(&String, &f32)> = partners.iter().collect();
        ranked.sort_by(|(b, wb), (d, wd)| {
            wd.partial_cmp(wb)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| b.cmp(d))
        });
        rows.extend(
            ranked
                .into_iter()
                .take(top_k)
                .map(|(file_b, weight)| CoChangeRow {
                    file_a: file_a.clone(),
                    file_b: file_b.clone(),
                    weight: *weight,
                }),
        );
    }
    rows.sort_by(|x, y| {
        x.file_a
            .cmp(&y.file_a)
            .then_with(|| x.file_b.cmp(&y.file_b))
    });
    rows
}

/// Retained co-change couplings per file (TASK-097): at most this many
/// rows per `file_a`, each direction independently, keeping storage linear
/// in files. A fixed retention constant, not configuration — it bounds
/// storage, not behavior (the bulk-exclusion threshold is the tunable).
pub const CO_CHANGE_TOP_K: usize = 10;

/// The mining knobs a caller passes through `mine_full`/`refresh`,
/// carried as one struct so adding a knob never touches the call sites
/// again (built from `[history]` via `From<&HistoryConfig>`).
#[derive(Debug, Clone, Copy)]
pub struct MiningOptions {
    /// Number of newest commits to mine.
    pub window: usize,
    /// Bulk-commit exclusion threshold for co-change derivation
    /// (PRD-HIST-REQ-005).
    pub max_commit_files: usize,
}

impl From<&crate::config::HistoryConfig> for MiningOptions {
    fn from(config: &crate::config::HistoryConfig) -> Self {
        Self {
            window: config.window,
            max_commit_files: config.max_commit_files,
        }
    }
}

/// Whether `repo_root` looks like a git work tree (a `.git` entry exists).
pub fn has_git(repo_root: &Path) -> bool {
    repo_root.join(".git").exists()
}

/// `git log` invocation shared by the full and incremental mines: the
/// newest `window` commits of `range` (None = HEAD), no renames, one TAB
/// header + `--name-only` paths per commit. The header carries the author
/// name as its third field (TASK-105: `%an`). `-c core.quotePath=false`
/// asks git for RAW non-ASCII paths — under the default it C-quotes every
/// path with a byte over 0x7f (`"src/caf\303\251.rs"`), which would never
/// match a real path at lookup time; paths git still quotes (quotes,
/// backslashes, control bytes) are unquoted by [`parse_git_log`]. `-n`
/// bounds cost by the window regardless of repository age
/// (PRD-HIST-REQ-002).
fn git_log(repo_root: &Path, window: usize, range: Option<&str>) -> Result<String> {
    let n = window.to_string();
    let mut args: Vec<&str> = vec![
        "-c",
        "core.quotePath=false",
        "log",
        "-n",
        &n,
        "--no-renames",
        "--format=%H%x09%ct%x09%an",
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

/// Mine the newest `opts.window` commits of HEAD history into the history
/// tables, replacing any previous mine.
pub fn mine_full(conn: &Connection, repo_root: &Path, opts: &MiningOptions) -> Result<()> {
    let head = current_head(repo_root);
    let commits = parse_git_log(&git_log(repo_root, opts.window, None)?);

    let tx = conn.unchecked_transaction()?;
    tx.execute_batch(
        "DELETE FROM commit_files;
         DELETE FROM mined_commits;
         DELETE FROM file_churn;
         DELETE FROM co_change;
         DELETE FROM history_meta;",
    )?;
    insert_commits(&tx, &commits)?;
    trim_to_window(&tx, opts.window)?;
    recompute_history_aggregates(&tx, opts)?;
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
///     the retained detail, trimmed to the window, and the aggregates are
///     RECOMPUTED from that detail — which rescales every retained
///     commit's age weight to the new window bounds exactly, without
///     re-reading git. A history rewrite that invalidates the stored head
///     falls back to ONE full re-mine; any git failure warns and returns
///     [`RefreshOutcome::Failed`] with the previous data retained
///     (PRD-HIST-REQ-008) — never an error.
pub fn refresh(
    conn: &Connection,
    repo_root: &Path,
    opts: &MiningOptions,
) -> Result<RefreshOutcome> {
    if !has_git(repo_root) {
        return Ok(RefreshOutcome::Skipped);
    }
    match refresh_inner(conn, repo_root, opts) {
        Ok(outcome) => Ok(outcome),
        Err(e) => {
            eprintln!("wonk: history refresh failed, keeping previous data: {e:#}");
            Ok(RefreshOutcome::Failed)
        }
    }
}

fn refresh_inner(
    conn: &Connection,
    repo_root: &Path,
    opts: &MiningOptions,
) -> Result<RefreshOutcome> {
    // The shared tail of every full-re-mine path below.
    let full_remine = |conn: &Connection| -> Result<RefreshOutcome> {
        mine_full(conn, repo_root, opts)?;
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
        match git_log(repo_root, opts.window, Some(&format!("{mined_head}..HEAD"))) {
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
    trim_to_window(&tx, opts.window)?;
    recompute_history_aggregates(&tx, opts)?;
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
            "INSERT OR IGNORE INTO mined_commits(commit_id, commit_ts, author) \
             VALUES (?1, ?2, ?3)",
            rusqlite::params![commit.id, commit.ts, commit.author],
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

/// Recompute `file_churn` AND `co_change` from the retained detail — the
/// one aggregate implementation. Rows are read newest-first (`commit_ts
/// DESC, commit_id DESC`) so the sum order — and therefore the stored
/// scores — is deterministic regardless of git's log order. Bounds come
/// from ALL retained commits (a file-less commit still bounds the window),
/// and weights are derived from the RETAINED window's own bounds, so
/// folding in new commits rescales every retained commit's weight exactly.
/// The co-change side additionally drops commits over
/// `opts.max_commit_files` (PRD-HIST-REQ-005) and keeps at most
/// [`CO_CHANGE_TOP_K`] partners per file (PRD-HIST-REQ-004).
fn recompute_history_aggregates(conn: &Connection, opts: &MiningOptions) -> Result<()> {
    let mut commits: Vec<MinedCommit> = {
        let mut stmt = conn.prepare(
            "SELECT commit_id, commit_ts, author FROM mined_commits \
             ORDER BY commit_ts DESC, commit_id DESC",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(MinedCommit {
                id: row.get(0)?,
                ts: row.get(1)?,
                author: row.get(2)?,
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
    let churn = aggregate_churn(&commits, head_ts, span);
    let file_history = aggregate_file_history(&commits, head_ts, span);
    let co_change = top_k_per_file(
        &aggregate_co_change(&commits, head_ts, span, opts.max_commit_files),
        CO_CHANGE_TOP_K,
    );

    conn.execute("DELETE FROM file_churn", [])?;
    {
        let mut churn_insert = conn.prepare(
            "INSERT INTO file_churn(file, score, last_ts, last_author, primary_author) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
        )?;
        for (file, score) in &churn {
            churn_insert.execute(rusqlite::params![
                file,
                score,
                file_history.get(file).map(|h| h.last_ts),
                file_history.get(file).and_then(|h| h.last_author.clone()),
                file_history.get(file).and_then(|h| h.primary_author.clone()),
            ])?;
        }
    }

    conn.execute("DELETE FROM co_change", [])?;
    let mut co_insert =
        conn.prepare("INSERT INTO co_change(file_a, file_b, weight) VALUES (?1, ?2, ?3)")?;
    for row in &co_change {
        co_insert.execute(rusqlite::params![row.file_a, row.file_b, row.weight])?;
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

    /// Mining options at the default bulk threshold, for tests that only
    /// care about the window.
    fn opts(window: usize) -> MiningOptions {
        MiningOptions {
            window,
            max_commit_files: 50,
        }
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

        mine_full(&conn, dir.path(), &opts(3)).unwrap();
        assert_eq!(mined_count(&conn), 3, "window smaller than history");

        mine_full(&conn, dir.path(), &opts(10)).unwrap();
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
        mine_full(&conn, dir.path(), &opts(4)).unwrap();

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
    fn mine_full_keys_special_filenames_by_their_real_paths() {
        if !git_available() {
            return;
        }
        // Under git's default core.quotePath=true a non-ASCII path is
        // emitted C-quoted (`"src/caf\303\251.rs"`), and a path carrying a
        // quote or backslash is ALWAYS quoted whatever quotePath says
        // (`"src/wei\"rd\\name.rs"`). Every one of them must be keyed in
        // file_churn under its REAL name — the churn signal looks up
        // `candidate.result.file`, never a quoted form.
        let (dir, conn) = make_history_repo(&[("src/café.rs", 100), ("src/wei\"rd\\name.rs", 200)]);
        mine_full(&conn, dir.path(), &opts(10)).unwrap();

        assert!(
            churn_score(&conn, "src/café.rs").is_some(),
            "unicode path must be keyed unquoted"
        );
        assert!(
            churn_score(&conn, "src/wei\"rd\\name.rs").is_some(),
            "quote/backslash path must be keyed unquoted"
        );
        // And nothing is keyed under a quoted form.
        let quoted: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM file_churn WHERE file LIKE '\"%'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(quoted, 0, "no file_churn row may start with a quote");
    }

    #[test]
    fn refresh_is_unchanged_when_head_stable() {
        if !git_available() {
            return;
        }
        let (dir, conn) = make_history_repo(&[("src/lib.rs", 100), ("src/lib.rs", 200)]);
        mine_full(&conn, dir.path(), &opts(10)).unwrap();
        let before = churn_score(&conn, "src/lib.rs").unwrap();
        let rows = mined_count(&conn);
        let head = mined_head(&conn);

        assert_eq!(
            refresh(&conn, dir.path(), &opts(10)).unwrap(),
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
        mine_full(&conn, dir.path(), &opts(3)).unwrap();
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
            refresh(&conn, dir.path(), &opts(3)).unwrap(),
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
        mine_full(&conn, dir.path(), &opts(3)).unwrap();

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

        let outcome = refresh(&conn, dir.path(), &opts(3)).unwrap();
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
        mine_full(&conn, dir.path(), &opts(10)).unwrap();
        let before = churn_score(&conn, "src/lib.rs").unwrap();

        // Corrupt the repository after a good mine.
        std::fs::write(dir.path().join(".git/HEAD"), "garbage\n").unwrap();
        assert_eq!(
            refresh(&conn, dir.path(), &opts(10)).unwrap(),
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
        mine_full(&conn, dir.path(), &opts(1)).unwrap();

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
            refresh(&conn, dir.path(), &opts(10)).unwrap(),
            RefreshOutcome::Skipped
        );
        assert_eq!(mined_count(&conn), 0);
    }

    // -- co-change persistence (TASK-097) --------------------------------------

    /// A grouped fixture: each `(&files, ts)` group becomes ONE dated commit
    /// touching every file in the group (the co-occurrence shape the
    /// single-file `make_history_repo` cannot express).
    fn make_history_repo_groups(groups: &[(&[&str], i64)]) -> (TempDir, Connection) {
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
        for (i, (files, ts)) in groups.iter().enumerate() {
            for file in *files {
                if let Some(parent) = Path::new(file).parent() {
                    std::fs::create_dir_all(root.join(parent)).unwrap();
                }
                std::fs::write(root.join(file), format!("content {i}\n")).unwrap();
            }
            let date = format!("@{ts} +0000");
            let ok = Command::new("git")
                .args(["add", "."])
                .current_dir(root)
                .output()
                .unwrap();
            assert!(ok.status.success());
            let ok = Command::new("git")
                .args(["commit", "-m", &format!("c{i}")])
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

    fn coupling(conn: &Connection, file_a: &str, file_b: &str) -> Option<f64> {
        conn.query_row(
            "SELECT weight FROM co_change WHERE file_a = ?1 AND file_b = ?2",
            rusqlite::params![file_a, file_b],
            |row| row.get(0),
        )
        .ok()
    }

    fn co_change_rows(conn: &Connection) -> Vec<(String, String, f64)> {
        let mut stmt = conn
            .prepare("SELECT file_a, file_b, weight FROM co_change ORDER BY file_a, file_b")
            .unwrap();
        let rows = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .unwrap();
        rows.collect::<rusqlite::Result<_>>().unwrap()
    }

    #[test]
    fn mine_full_populates_symmetric_exact_co_change_weights() {
        if !git_available() {
            return;
        }
        let (dir, conn) = make_history_repo_groups(&[
            (&["handler.rs", "serializer.rs"], 100),
            (&["handler.rs", "serializer.rs", "third.rs"], 50),
        ]);
        mine_full(&conn, dir.path(), &opts(10)).unwrap();

        // head 100, span 50: the ts=100 pair weighs 1.0, the ts=50 one 0.0.
        let w = coupling(&conn, "handler.rs", "serializer.rs").unwrap();
        assert!((w - 1.0).abs() < 1e-6, "got {w}");
        assert_eq!(
            coupling(&conn, "serializer.rs", "handler.rs"),
            Some(w),
            "both directions carry the same weight"
        );
        assert!(
            coupling(&conn, "handler.rs", "third.rs").is_some(),
            "zero-weight pairs are retained like churn's zero scores"
        );
    }

    #[test]
    fn refresh_rescales_retained_co_change_weights() {
        if !git_available() {
            return;
        }
        let (dir, conn) = make_history_repo_groups(&[
            (&["a.rs", "b.rs"], 100),
            (&["a.rs", "b.rs"], 110),
            (&["a.rs", "b.rs"], 120),
        ]);
        mine_full(&conn, dir.path(), &opts(3)).unwrap();
        // span 20: (a,b) = 0 + 0.5 + 1.0 = 1.5.
        assert!((coupling(&conn, "a.rs", "b.rs").unwrap() - 1.5).abs() < 1e-6);

        let date = "@130 +0000";
        std::fs::write(dir.path().join("c.rs"), "new\n").unwrap();
        // Add exactly the new file: the fixture's own history.db lives in
        // the working tree, and `git add .` would swallow it into the
        // commit, coupling c.rs to the index file.
        Command::new("git")
            .args(["add", "c.rs"])
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
            refresh(&conn, dir.path(), &opts(3)).unwrap(),
            RefreshOutcome::Refreshed(1)
        );

        // Retained window {110, 120, 130}: (a,b) RESCALED to 0 + 0.5 = 0.5
        // (an append-only weight would still read 1.5); c.rs is a loner.
        assert!((coupling(&conn, "a.rs", "b.rs").unwrap() - 0.5).abs() < 1e-6);
        assert!(
            co_change_rows(&conn)
                .iter()
                .all(|(a, b, _)| a != "c.rs" && b != "c.rs")
        );
    }

    #[test]
    fn refresh_unchanged_leaves_co_change_byte_identical() {
        if !git_available() {
            return;
        }
        let (dir, conn) = make_history_repo_groups(&[
            (&["a.rs", "b.rs"], 100),
            (&["a.rs", "c.rs"], 150),
            (&["b.rs", "c.rs", "d.rs"], 200),
        ]);
        mine_full(&conn, dir.path(), &opts(10)).unwrap();
        let before = co_change_rows(&conn);
        assert!(!before.is_empty());

        assert_eq!(
            refresh(&conn, dir.path(), &opts(10)).unwrap(),
            RefreshOutcome::Unchanged
        );
        assert_eq!(co_change_rows(&conn), before);
    }

    #[test]
    fn co_change_top_k_constant_is_ten() {
        assert_eq!(CO_CHANGE_TOP_K, 10);
    }

    /// The reformatting-sweep shape: `[a.rs, b.rs]@100` then a commit
    /// sweeping `a.rs` plus `f000..f499` at 200.
    fn make_reformat_repo(with_pair_commit: bool) -> (TempDir, Connection) {
        let owned: Vec<String> = (0..500).map(|i| format!("f{i:03}.rs")).collect();
        let mut sweep: Vec<&str> = vec!["a.rs"];
        sweep.extend(owned.iter().map(String::as_str));
        let groups: Vec<(&[&str], i64)> = if with_pair_commit {
            vec![(&["a.rs", "b.rs"], 100), (&sweep, 200)]
        } else {
            vec![(&sweep, 200)]
        };
        make_history_repo_groups(&groups)
    }

    #[test]
    fn ac2_500_file_reformat_commit_produces_no_coupling() {
        if !git_available() {
            return;
        }
        let (dir, conn) = make_reformat_repo(true);
        mine_full(&conn, dir.path(), &opts(10)).unwrap();

        // No swept file is coupled to anything, in either direction.
        let swept: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM co_change \
                 WHERE file_a LIKE 'f%' OR file_b LIKE 'f%'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(swept, 0, "a 500-file sweep must derive no coupling");

        // The pair below it keeps its exact age weight (the ts=100 commit
        // is the only non-bulk co-occurrence; head 200, span 100 → 0.0).
        assert_eq!(
            coupling(&conn, "a.rs", "b.rs"),
            Some(age_weight(100, 200, 100) as f64)
        );

        // Exclusion is co-change ONLY: the sweep still feeds churn
        // (a.rs = 0.0 from ts=100 + 1.0 from the sweep; b.rs = 0.0).
        assert!((churn_score(&conn, "a.rs").unwrap() - 1.0).abs() < 1e-6);
        assert_eq!(mined_count(&conn), 2, "the sweep still bounds the window");
    }

    #[test]
    fn ac2_reformat_only_repo_stores_no_coupling_rows() {
        if !git_available() {
            return;
        }
        let (dir, conn) = make_reformat_repo(false);
        mine_full(&conn, dir.path(), &opts(10)).unwrap();

        let total: i64 = conn
            .query_row("SELECT COUNT(*) FROM co_change", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            total, 0,
            "a repo whose only commit is a sweep has no coupling"
        );
        // The sweep itself was still mined.
        assert_eq!(mined_count(&conn), 1);
    }

    #[test]
    fn bulk_threshold_is_configurable_boundary() {
        if !git_available() {
            return;
        }
        let four = make_history_repo_groups(&[(&["a.rs", "b.rs", "c.rs", "d.rs"], 100)]);
        mine_full(
            &four.1,
            four.0.path(),
            &MiningOptions {
                window: 10,
                max_commit_files: 3,
            },
        )
        .unwrap();
        assert!(
            co_change_rows(&four.1).is_empty(),
            "4 files > max_commit_files=3: excluded"
        );

        let three = make_history_repo_groups(&[(&["a.rs", "b.rs", "c.rs"], 100)]);
        mine_full(
            &three.1,
            three.0.path(),
            &MiningOptions {
                window: 10,
                max_commit_files: 3,
            },
        )
        .unwrap();
        // 3 files == threshold: every ordered pair retained (3*2).
        assert_eq!(co_change_rows(&three.1).len(), 6);
        assert!((coupling(&three.1, "a.rs", "b.rs").unwrap() - 1.0).abs() < 1e-6);
    }

    #[test]
    fn ac3_storage_is_linear_top_k_per_file() {
        if !git_available() {
            return;
        }
        let owned: Vec<String> = (0..12).map(|i| format!("f{i:02}.rs")).collect();
        let files: Vec<&str> = owned.iter().map(String::as_str).collect();
        let (dir, conn) = make_history_repo_groups(&[(&files, 100), (&files, 200)]);
        mine_full(&conn, dir.path(), &opts(10)).unwrap();

        // 12 files x 11 equal-weight partners each: top-K keeps exactly
        // K per file_a — 120 rows, linear in files, never 12*11.
        let total: i64 = conn
            .query_row("SELECT COUNT(*) FROM co_change", [], |r| r.get(0))
            .unwrap();
        assert_eq!(total, 120, "12 files x top-10 = 120 rows exactly");

        // The tie-break (weight equal, file_b ASC) drops each file's
        // lexicographically-largest partner — deterministic.
        let per_file: Vec<(String, i64)> = {
            let mut stmt = conn
                .prepare("SELECT file_a, COUNT(*) FROM co_change GROUP BY file_a")
                .unwrap();
            let rows = stmt
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
                .unwrap();
            rows.collect::<rusqlite::Result<_>>().unwrap()
        };
        assert_eq!(per_file.len(), 12);
        assert!(per_file.iter().all(|(_, n)| *n == 10));
        assert!(
            coupling(&conn, "f00.rs", "f11.rs").is_none(),
            "largest dropped"
        );
        assert!(
            coupling(&conn, "f11.rs", "f10.rs").is_none(),
            "largest dropped"
        );
        assert!(coupling(&conn, "f11.rs", "f09.rs").is_some(), "rest kept");
        assert!(
            coupling(&conn, "f05.rs", "f11.rs").is_none(),
            "largest dropped"
        );
    }

    #[test]
    fn ac1_file_repeatedly_changing_alongside_query_target_is_surfaced() {
        if !git_available() {
            return;
        }
        // handler.rs and serializer.rs change together five times; loner.rs
        // changes alone five times at the same moments — equally hot by
        // churn, coupled to nothing (TASK-097, PRD-HIST-REQ-006).
        let mut groups: Vec<(&[&str], i64)> = Vec::new();
        for i in 0..5 {
            let ts = 110 + i;
            groups.push((&["handler.rs", "serializer.rs"], ts));
            groups.push((&["loner.rs"], ts));
        }
        let (dir, conn) = make_history_repo_groups(&groups);
        mine_full(&conn, dir.path(), &opts(10)).unwrap();

        // Three same-category candidates, all containing the query target:
        // only the co-change signal separates them.
        let hits = ["handler.rs", "serializer.rs", "loner.rs"]
            .into_iter()
            .map(|file| crate::search::SearchResult {
                file: Path::new(file).to_path_buf(),
                line: 1,
                col: 1,
                content: format!("{file} target"),
            })
            .collect::<Vec<_>>();
        let scored = crate::rerank::rerank(
            crate::ranker::classify_results(&hits, None),
            &crate::rerank::QueryInfo { pattern: "target" },
            Some(&conn),
            &crate::rerank::WeightTable::from_pairs([("co_change".to_string(), 1.0f32)]).unwrap(),
            &crate::rerank::ContextSources::default(),
        );

        let by_file = |f: &str| {
            scored
                .iter()
                .find(|s| s.classified.result.file == Path::new(f))
                .unwrap()
        };
        let coupled: Vec<&str> = ["handler.rs", "serializer.rs"]
            .into_iter()
            .filter(|f| by_file(f).score > by_file("loner.rs").score)
            .collect();
        assert_eq!(coupled.len(), 2, "both co-moving files outrank the loner");
        for f in ["handler.rs", "serializer.rs"] {
            let contribution = by_file(f)
                .contributions
                .iter()
                .find(|c| c.signal == "co_change")
                .unwrap();
            assert!(contribution.value > 0.0, "{f} has positive evidence");
            assert!(
                (contribution.value - 1.0).abs() < 1e-6,
                "{f} carries the set max: {}",
                contribution.value
            );
        }
        let loner = by_file("loner.rs");
        let loner_contribution = loner
            .contributions
            .iter()
            .find(|c| c.signal == "co_change")
            .unwrap();
        assert_eq!(
            loner_contribution.value, 0.0,
            "the loner has no coupling evidence"
        );
        assert_eq!(loner.score, 0.0);
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
                author: None,
                files: vec!["src/lib.rs".to_string(), "src/my file.rs".to_string()]
            }
        );
        assert_eq!(
            commits[1],
            MinedCommit {
                id: "bbbbbbbb".to_string(),
                ts: 50,
                author: None,
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
        // The defensive (non-git) form: a raw-TAB path must not be
        // mistaken for a commit header. Real git never emits this — it
        // C-quotes control bytes (see the quoted form below).
        let commits = parse_git_log("aaaa\t10\nweird\tname.rs\n");
        assert_eq!(commits.len(), 1);
        assert_eq!(commits[0].files, vec!["weird\tname.rs".to_string()]);
    }

    #[test]
    fn parse_git_log_unquotes_c_style_quoted_paths() {
        // Exactly what `git log --name-only` emits for these filenames
        // under the default core.quotePath=true: the non-ASCII path as
        // octal byte escapes, the quote/backslash path escaped in place.
        let log = "aaaaaaaa\t100\n\"src/caf\\303\\251.rs\"\n\"src/wei\\\"rd\\\\name.rs\"\n";
        let commits = parse_git_log(log);
        assert_eq!(commits.len(), 1);
        assert_eq!(
            commits[0].files,
            vec![
                "src/café.rs".to_string(),
                "src/wei\"rd\\name.rs".to_string(),
            ]
        );
    }

    #[test]
    fn parse_git_log_quoted_tab_path_decodes_to_a_real_tab() {
        // A tab-containing path — the realistic quoted form git emits even
        // with core.quotePath=false. The escaped tab keeps the line out of
        // the header branch, and the decode yields a real TAB.
        let commits = parse_git_log("aaaa\t10\n\"weird\\tname.rs\"\n");
        assert_eq!(commits.len(), 1);
        assert_eq!(commits[0].files, vec!["weird\tname.rs".to_string()]);
    }

    #[test]
    fn parse_git_log_leaves_unquoted_and_unknown_escapes_verbatim() {
        // A plain path (quotePath=false emits these) stays untouched, and
        // an unrecognized escape inside quotes is kept as written rather
        // than guessed at.
        let commits = parse_git_log("aaaa\t10\nsrc/plain.rs\n\"odd\\q.rs\"\n");
        assert_eq!(commits.len(), 1);
        assert_eq!(
            commits[0].files,
            vec!["src/plain.rs".to_string(), "odd\\q.rs".to_string()]
        );
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
            author: None,
            files: vec![],
        }];
        assert_eq!(window_bounds(&one), (100, 0));
        let three = vec![
            MinedCommit {
                id: "a".into(),
                ts: 100,
                author: None,
                files: vec![],
            },
            MinedCommit {
                id: "b".into(),
                ts: 75,
                author: None,
                files: vec![],
            },
            MinedCommit {
                id: "c".into(),
                ts: 50,
                author: None,
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
                author: None,
                files: vec!["hot.rs".into()],
            },
            MinedCommit {
                id: "b".into(),
                ts: 75,
                author: None,
                files: vec!["hot.rs".into(), "mid.rs".into()],
            },
            MinedCommit {
                id: "c".into(),
                ts: 50,
                author: None,
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

    // -- aggregate_co_change / top_k_per_file (TASK-097) ---------------------

    fn commit(id: &str, ts: i64, files: &[&str]) -> MinedCommit {
        MinedCommit {
            id: id.to_string(),
            ts,
            author: None,
            files: files.iter().map(|f| f.to_string()).collect(),
        }
    }

    #[test]
    fn aggregate_co_change_sums_age_weights_per_ordered_pair() {
        let rows = vec![
            commit("a", 100, &["handler.rs", "serializer.rs"]),
            commit("b", 50, &["handler.rs", "serializer.rs"]),
            commit("c", 50, &["handler.rs", "loner.rs"]),
        ];
        let weights = aggregate_co_change(&rows, 100, 50, 50);

        // Only the newest commit weighs 1.0; the ts=50 commits weigh 0.0.
        assert!((weights["handler.rs"]["serializer.rs"] - 1.0).abs() < 1e-6);
        assert!(
            (weights["serializer.rs"]["handler.rs"] - 1.0).abs() < 1e-6,
            "symmetric by construction"
        );
        assert!((weights["handler.rs"]["loner.rs"] - 0.0).abs() < 1e-6);
        assert_eq!(weights.len(), 3);
    }

    #[test]
    fn aggregate_co_change_solo_commit_yields_no_pairs() {
        let rows = vec![commit("a", 100, &["one.rs"]), commit("b", 50, &[])];
        let weights = aggregate_co_change(&rows, 100, 50, 50);
        assert!(weights.is_empty(), "got {weights:?}");
    }

    #[test]
    fn aggregate_co_change_never_pairs_a_file_with_itself() {
        // A duplicated path in one commit's file list (defensive: the
        // commit_files primary key rules it out in practice) must not
        // create a self-coupling.
        let rows = vec![commit("a", 100, &["x.rs", "x.rs"])];
        let weights = aggregate_co_change(&rows, 100, 50, 50);
        assert!(weights.is_empty(), "got {weights:?}");
    }

    #[test]
    fn aggregate_co_change_excludes_commits_strictly_over_the_threshold() {
        let rows = vec![
            commit("at", 100, &["a.rs", "b.rs", "c.rs"]),
            commit("over", 50, &["a.rs", "b.rs", "d.rs", "e.rs"]),
        ];
        let weights = aggregate_co_change(&rows, 100, 50, 3);

        // Exactly-at-threshold contributes (span: weights 1.0 and 0.0).
        assert!((weights["a.rs"]["b.rs"] - 1.0).abs() < 1e-6);
        assert!((weights["a.rs"]["c.rs"] - 1.0).abs() < 1e-6);
        assert!((weights["b.rs"]["c.rs"] - 1.0).abs() < 1e-6);
        // Strictly-over contributes nothing: d and e have no coupling.
        assert!(!weights.contains_key("d.rs"));
        assert!(!weights.contains_key("e.rs"));
    }

    #[test]
    fn top_k_per_file_keeps_k_strongest_ties_break_file_b_asc() {
        let mut weights: HashMap<String, HashMap<String, f32>> = HashMap::new();
        let mut partners = HashMap::new();
        partners.insert("b.rs".to_string(), 0.5);
        partners.insert("c.rs".to_string(), 1.0);
        partners.insert("d.rs".to_string(), 0.5);
        weights.insert("a.rs".to_string(), partners);

        let rows = top_k_per_file(&weights, 2);
        assert_eq!(
            rows,
            vec![
                CoChangeRow {
                    file_a: "a.rs".into(),
                    file_b: "b.rs".into(),
                    weight: 0.5
                },
                CoChangeRow {
                    file_a: "a.rs".into(),
                    file_b: "c.rs".into(),
                    weight: 1.0
                },
            ],
            "K=2 keeps the strongest (c) and the ASC tie-break (b over d)"
        );
    }

    #[test]
    fn top_k_per_file_at_least_partner_count_keeps_all() {
        let mut weights: HashMap<String, HashMap<String, f32>> = HashMap::new();
        let mut a_partners = HashMap::new();
        a_partners.insert("b.rs".to_string(), 1.0);
        weights.insert("a.rs".to_string(), a_partners);
        let mut b_partners = HashMap::new();
        b_partners.insert("a.rs".to_string(), 1.0);
        weights.insert("b.rs".to_string(), b_partners);

        let rows = top_k_per_file(&weights, 10);
        assert_eq!(
            rows,
            vec![
                CoChangeRow {
                    file_a: "a.rs".into(),
                    file_b: "b.rs".into(),
                    weight: 1.0
                },
                CoChangeRow {
                    file_a: "b.rs".into(),
                    file_b: "a.rs".into(),
                    weight: 1.0
                },
            ],
            "rows are emitted sorted by (file_a, file_b)"
        );
    }

    #[test]
    fn top_k_per_file_zero_k_yields_nothing() {
        let mut weights: HashMap<String, HashMap<String, f32>> = HashMap::new();
        let mut partners = HashMap::new();
        partners.insert("b.rs".to_string(), 1.0);
        weights.insert("a.rs".to_string(), partners);
        assert!(top_k_per_file(&weights, 0).is_empty());
        assert!(top_k_per_file(&HashMap::new(), 10).is_empty());
    }

    #[test]
    fn top_k_per_file_selects_each_direction_independently() {
        // a's strongest partner is b, so a DROPS c; c's only partner is a,
        // so c KEEPS a — (c,a) exists while (a,c) does not.
        let mut weights: HashMap<String, HashMap<String, f32>> = HashMap::new();
        let mut a_partners = HashMap::new();
        a_partners.insert("b.rs".to_string(), 2.0);
        a_partners.insert("c.rs".to_string(), 1.0);
        weights.insert("a.rs".to_string(), a_partners);
        let mut c_partners = HashMap::new();
        c_partners.insert("a.rs".to_string(), 1.0);
        weights.insert("c.rs".to_string(), c_partners);

        let rows = top_k_per_file(&weights, 1);
        let pairs: Vec<(&str, &str)> = rows
            .iter()
            .map(|r| (r.file_a.as_str(), r.file_b.as_str()))
            .collect();
        assert_eq!(pairs, vec![("a.rs", "b.rs"), ("c.rs", "a.rs")]);
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
        mine_full(&conn, dir.path(), &opts(500)).unwrap();
        let elapsed = start.elapsed();
        assert_eq!(mined_count(&conn), 500, "whole history within the window");
        assert!(
            elapsed.as_secs_f64() < 10.0,
            "window=500 mine exceeded the smoke budget: {elapsed:?}"
        );

        // The structural bound: cost is proportional to the window because
        // git reads at most `window` commits — window=50 retains EXACTLY 50
        // rows of the same 500-commit history.
        mine_full(&conn, dir.path(), &opts(50)).unwrap();
        assert_eq!(mined_count(&conn), 50);
        let short_map = full_churn_map(&conn);

        // Cost is not repo age: a LONGER history whose newest 50 commits
        // are identical (same timestamps, same files) retains the identical
        // rows and the identical aggregate.
        let longer = make_dated_repo(550, 999_999_950);
        let conn_long = crate::db::open(&longer.path().join("ac2b.db")).unwrap();
        mine_full(&conn_long, longer.path(), &opts(50)).unwrap();
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

        mine_full(&conn, dir.path(), &opts(3)).unwrap();
        assert_eq!(mined_count(&conn), 3);
        let tight = full_churn_map(&conn);

        mine_full(&conn, dir.path(), &opts(50)).unwrap();
        assert_eq!(mined_count(&conn), 50);
        let wide = full_churn_map(&conn);

        assert_ne!(
            tight, wide,
            "the window size must change the mined aggregate"
        );
    }

    // -- authors + per-file history (TASK-105) --------------------------------

    #[test]
    fn parse_git_log_parses_three_field_header_with_author() {
        let commits = parse_git_log("aaaaaaaa\t100\tAda Lovelace\nsrc/lib.rs\n");
        assert_eq!(commits.len(), 1);
        assert_eq!(commits[0].author.as_deref(), Some("Ada Lovelace"));
        assert_eq!(commits[0].ts, 100);
        assert_eq!(commits[0].files, vec!["src/lib.rs".to_string()]);
    }

    #[test]
    fn parse_git_log_empty_author_field_is_none() {
        let commits = parse_git_log("aaaaaaaa\t100\t\nsrc/lib.rs\n");
        assert_eq!(commits.len(), 1, "the empty third field is still a header");
        assert_eq!(commits[0].author, None);
    }

    #[test]
    fn parse_git_log_two_field_legacy_header_has_no_author() {
        // Old stored detail and hand-written fixtures lack the author field;
        // the header still parses with `author: None`.
        let commits = parse_git_log("aaaaaaaa\t100\nsrc/lib.rs\n");
        assert_eq!(commits.len(), 1);
        assert_eq!(commits[0].author, None);
    }

    fn commit_with_author(id: &str, ts: i64, author: &str, files: &[&str]) -> MinedCommit {
        MinedCommit {
            id: id.to_string(),
            ts,
            author: Some(author.to_string()),
            files: files.iter().map(|f| f.to_string()).collect(),
        }
    }

    #[test]
    fn aggregate_file_history_takes_the_newest_sighting() {
        // Rows newest-first, the order recompute reads them.
        let rows = vec![
            commit_with_author("c2", 300, "Bob", &["a.rs"]),
            commit_with_author("c1", 100, "Ada", &["a.rs"]),
        ];
        let history = aggregate_file_history(&rows, 300, 200);
        let a = &history["a.rs"];
        assert_eq!(a.last_ts, 300);
        assert_eq!(a.last_author.as_deref(), Some("Bob"), "newest commit's author");
    }

    #[test]
    fn aggregate_file_history_primary_author_by_weighted_count() {
        // head 300, span 200: Zed's ts=300 commit weighs 1.0; Ada's ts=200
        // commit weighs 0.5 — Zed is primary despite one commit each.
        let rows = vec![
            commit_with_author("c2", 300, "Zed", &["a.rs"]),
            commit_with_author("c1", 200, "Ada", &["a.rs"]),
        ];
        let history = aggregate_file_history(&rows, 300, 200);
        assert_eq!(history["a.rs"].primary_author.as_deref(), Some("Zed"));
    }

    #[test]
    fn aggregate_file_history_tie_breaks_to_smallest_author_name() {
        // Same timestamp: equal weights, so the lexicographically smallest
        // author name wins — a total order, never HashMap iteration order.
        let rows = vec![
            commit_with_author("c2", 100, "Zed", &["a.rs"]),
            commit_with_author("c1", 100, "Ada", &["a.rs"]),
            commit_with_author("c0", 100, "Mid", &["a.rs"]),
        ];
        let history = aggregate_file_history(&rows, 100, 0);
        assert_eq!(history["a.rs"].primary_author.as_deref(), Some("Ada"));
    }

    #[test]
    fn aggregate_file_history_authorless_commits_update_ts_only() {
        let rows = vec![
            MinedCommit {
                id: "c2".into(),
                ts: 300,
                author: None,
                files: vec!["a.rs".into()],
            },
            commit_with_author("c1", 100, "Ada", &["a.rs"]),
        ];
        let history = aggregate_file_history(&rows, 300, 200);
        let a = &history["a.rs"];
        assert_eq!(a.last_ts, 300, "the newest commit still fixes last_ts");
        assert_eq!(a.last_author, None, "an authorless newest commit");
        assert_eq!(a.primary_author.as_deref(), Some("Ada"), "Ada is the only author");
    }

    #[test]
    fn aggregate_file_history_empty_is_empty() {
        assert!(aggregate_file_history(&[], 0, 0).is_empty());
    }

    /// The five `file_churn` columns after a recompute over directly seeded
    /// detail rows: `(score, last_ts, last_author, primary_author)`.
    fn file_churn_row(
        conn: &Connection,
        file: &str,
    ) -> (f64, Option<i64>, Option<String>, Option<String>) {
        conn.query_row(
            "SELECT score, last_ts, last_author, primary_author FROM file_churn WHERE file = ?1",
            rusqlite::params![file],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap()
    }

    #[test]
    fn recompute_history_aggregates_writes_per_file_history_columns() {
        let dir = TempDir::new().unwrap();
        let conn = crate::db::open(&dir.path().join("h.db")).unwrap();
        let commits = vec![
            commit_with_author("c2", 300, "Bob", &["a.rs", "b.rs"]),
            commit_with_author("c1", 100, "Ada", &["a.rs"]),
        ];
        insert_commits(&conn, &commits).unwrap();
        recompute_history_aggregates(&conn, &opts(10)).unwrap();

        // a.rs: last touched by Bob at 300; weighted counts Bob 1.0, Ada 0.0.
        let (score_a, last_ts, last_author, primary) = file_churn_row(&conn, "a.rs");
        assert!((score_a - 1.0).abs() < 1e-6, "got {score_a}");
        assert_eq!(last_ts, Some(300));
        assert_eq!(last_author.as_deref(), Some("Bob"));
        assert_eq!(primary.as_deref(), Some("Bob"));
        // b.rs: only Bob ever touched it.
        let (_, last_ts_b, last_author_b, primary_b) = file_churn_row(&conn, "b.rs");
        assert_eq!(last_ts_b, Some(300));
        assert_eq!(last_author_b.as_deref(), Some("Bob"));
        assert_eq!(primary_b.as_deref(), Some("Bob"));
    }

    #[test]
    fn recompute_history_aggregates_tolerates_authorless_detail() {
        let dir = TempDir::new().unwrap();
        let conn = crate::db::open(&dir.path().join("h.db")).unwrap();
        let commits = vec![MinedCommit {
            id: "c0".into(),
            ts: 100,
            author: None,
            files: vec!["a.rs".into()],
        }];
        insert_commits(&conn, &commits).unwrap();
        recompute_history_aggregates(&conn, &opts(10)).unwrap();

        let (_, last_ts, last_author, primary) = file_churn_row(&conn, "a.rs");
        assert_eq!(last_ts, Some(100));
        assert_eq!(last_author, None);
        assert_eq!(primary, None);
    }
}
