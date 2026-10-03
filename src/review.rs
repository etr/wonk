//! Diff-scoped review engine (TASK-085, widened 089).
//!
//! Pure composition over existing primitives: review never computes impact
//! of its own — every affected-symbol set is [`crate::blast::analyze_blast`]'s
//! own output, so `wonk review` and `wonk blast` cannot disagree. Findings
//! are emitted only (DR-035): no forge posting, no auto-fix.
//!
//! Every finding carries a line-independent identity over its
//! whitespace-folded anchored line (PRD-REV-REQ-013), consulted against the
//! durable `review_suppressions` table (REQ-014), and every produced finding
//! is either kept or dropped for exactly one counted reason (REQ-015).
//!
//! Index currency caveat (inherited V4 semantics): the index must reflect
//! the base state of the diff. Re-indexing mid-diff (auto-indexing the
//! current tree) empties the diff and fakes an APPROVE, which is why the
//! review dispatch never auto-initializes an index.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use anyhow::Result;
use rusqlite::Connection;
use sha2::{Digest, Sha256};

use crate::blast::{self, BlastOptions};
use crate::impact;
use crate::types::{
    AnchorMethod, BlastAffectedSymbol, BlastAnalysis, BlastDirection, BlastSeverity, ChangeScope,
    ChangedSymbol, FileDiffHunks, Finding, FindingSeverity, ReviewVerdict, Symbol, SymbolRef,
};

// ---------------------------------------------------------------------------
// Anchor resolution (PRD-REV-REQ-011/012)
// ---------------------------------------------------------------------------

fn range_covers(ranges: &[(usize, usize)], line: usize) -> bool {
    ranges
        .iter()
        .any(|&(start, end)| line >= start && line <= end)
}

/// Resolve a finding's anchor line for a changed symbol, recording which
/// tier resolved it.
///
/// Tier order: new-side hunk → old-side removed line → post-change file →
/// unresolved. For non-removed changes the line refers to the post-change
/// file; for [`AnchorMethod::OldSideLine`] it refers to the PRE-change file
/// (where the symbol used to live).
///
/// The hunk-path `ChangedSymbol::line` is the INDEXED (stale) line, so tier 3
/// deliberately re-resolves the symbol's line from the current file instead
/// of trusting it.
pub fn resolve_anchor(
    cs: &ChangedSymbol,
    hunks: Option<&FileDiffHunks>,
    current_symbols: Option<&[Symbol]>,
) -> (Option<usize>, AnchorMethod) {
    if cs.change_type == crate::types::ChangeType::Removed {
        // Tier 2: the index reflects the diff's base state, so the indexed
        // line is an old-side line. Anchor only if this diff's removed
        // ranges actually cover it; otherwise an honest no-line beats a
        // wrong line.
        return match hunks
            .filter(|h| range_covers(&h.removed_ranges, cs.line))
            .map(|_| cs.line)
        {
            Some(line) => (Some(line), AnchorMethod::OldSideLine),
            None => (None, AnchorMethod::Unresolved),
        };
    }

    // Re-resolve the symbol's start line in the post-change file: the
    // indexed line is stale after insertions above the symbol.
    let Some(cur_line) = current_symbols.and_then(|syms| {
        syms.iter()
            .filter(|s| s.name == cs.name && s.kind == cs.kind)
            .map(|s| s.line)
            .min()
    }) else {
        return (None, AnchorMethod::Unresolved);
    };

    // Tier 1: the diff touched the symbol's current lines.
    if hunks.is_some_and(|h| range_covers(&h.new_ranges, cur_line)) {
        return (Some(cur_line), AnchorMethod::NewSideHunk);
    }

    // Tier 3: the symbol exists post-change but its lines were not touched
    // (body-only hunk-overlap modifications land here).
    (Some(cur_line), AnchorMethod::PostChangeFile)
}

/// The text of the line the anchor resolved against (REQ-013's identity
/// input).
///
/// The side follows the tier: tiers 1/3 read the POST-change working-tree
/// file (`current_lines`, the same side `resolve_anchor` resolved the line
/// on); [`AnchorMethod::OldSideLine`] reads the PRE-change side — the
/// removed `-` lines the impact diff already carries. Unresolved anchors
/// have no text.
pub fn anchored_line_text(
    anchor_method: AnchorMethod,
    line: Option<usize>,
    hunks: Option<&FileDiffHunks>,
    current_lines: Option<&[String]>,
) -> Option<String> {
    match anchor_method {
        AnchorMethod::Unresolved => None,
        AnchorMethod::OldSideLine => {
            line.and_then(|n| hunks.and_then(|h| h.removed_lines.get(&n)).cloned())
        }
        AnchorMethod::NewSideHunk | AnchorMethod::PostChangeFile => line.and_then(|n| {
            current_lines
                .and_then(|lines| lines.get(n.checked_sub(1)?))
                .cloned()
        }),
    }
}

// ---------------------------------------------------------------------------
// Verdict (PRD-REV-REQ-005)
// ---------------------------------------------------------------------------

/// Derive the verdict mechanically from findings: any blocking → BLOCK, any
/// warning → REVIEW, else APPROVE. Pure — the only verdict path.
pub fn derive_verdict(findings: &[Finding]) -> ReviewVerdict {
    use crate::types::FindingSeverity;

    if findings
        .iter()
        .any(|f| f.severity == FindingSeverity::Blocking)
    {
        ReviewVerdict::Block
    } else if findings
        .iter()
        .any(|f| f.severity == FindingSeverity::Warning)
    {
        ReviewVerdict::Review
    } else {
        ReviewVerdict::Approve
    }
}

// ---------------------------------------------------------------------------
// Options and result (AR-022: each rule family independently disable-able)
// ---------------------------------------------------------------------------

/// Options for [`run_review`].
#[derive(Debug, Clone)]
pub struct ReviewOptions {
    /// Rule family A: breaking change (removed/signature-changed with live
    /// indexed callers).
    pub breaking_change: bool,
    /// Rule family B: coverage gap (no test in the blast radius).
    pub coverage_gap: bool,
    /// Rule family C: cross-repo contract impact (changed symbol provides a
    /// contract consumed by another indexed repo).
    pub cross_repo: bool,
    /// Whether qualifying blast queries may use the precomputed reach table
    /// (`[reach] enabled`); a kill switch for speed only, never findings.
    pub reach_enabled: bool,
    /// Blast traversal depth.
    pub depth: usize,
    /// Drop findings whose confidence is below this floor
    /// (PRD-REV-REQ-015). `None` keeps everything.
    pub min_confidence: Option<f64>,
    /// Drop findings less severe than this floor (PRD-REV-REQ-015).
    /// `None` keeps everything.
    pub min_severity: Option<FindingSeverity>,
    /// Keep only these finding categories (the `kind` field). Empty keeps
    /// every category (PRD-REV-REQ-015).
    pub kinds: Vec<String>,
    /// Keep at most this many findings after ranking — the cap trims the
    /// least severe and least confident first (PRD-REV-REQ-015). `None`
    /// keeps everything.
    pub max_findings: Option<usize>,
    /// Elide function bodies in source output (PRD-ELIDE-REQ-008 uniform
    /// surface). Inert on review's finding payload — no source bodies are
    /// emitted; the recorded reduction figure lives in bench/elision-results.md.
    pub elide: Option<crate::elide::Mode>,
}

impl Default for ReviewOptions {
    fn default() -> Self {
        Self {
            breaking_change: true,
            coverage_gap: true,
            cross_repo: true,
            reach_enabled: true,
            depth: blast::DEFAULT_DEPTH,
            min_confidence: None,
            min_severity: None,
            kinds: Vec::new(),
            max_findings: None,
            elide: None,
        }
    }
}

impl ReviewOptions {
    /// The config-derived rule switches shared verbatim by both dispatch
    /// surfaces (CLI `dispatch_review`, MCP `tool_review` — TASK-086
    /// review debt: the block was duplicated and a fourth rule family
    /// would have had to be wired in two places). Caller-only knobs (CLI
    /// filter flags, MCP elide) spread over the result.
    pub fn from_config(config: &crate::config::Config) -> Self {
        Self {
            breaking_change: config.review.breaking_change,
            coverage_gap: config.review.coverage_gap,
            cross_repo: config.review.cross_repo,
            reach_enabled: config.reach.enabled,
            ..Self::default()
        }
    }
}

/// Explicit cross-repo inputs for a review run (TASK-086).
///
/// The registry directory is a parameter, never re-derived from `$HOME`
/// inside the engine — a test run must never touch the caller's real
/// `~/.wonk/repos` registry, and the CLI/MCP resolve it once via
/// [`CrossRepoInputs::discover`].
#[derive(Debug, Clone)]
pub struct CrossRepoInputs {
    /// The reviewed repo's declared workspace ids
    /// (`[contracts] workspace`).
    pub declared: Vec<String>,
    /// Registry directory holding the sibling indexes to resolve against.
    pub repos_dir: std::path::PathBuf,
}

impl CrossRepoInputs {
    /// Discover the inputs for `repo_root`: the repo's declared workspace
    /// plus the default registry directory. `None` when no home directory
    /// exists — cross-repo review is then simply unavailable and
    /// [`run_review`] degrades with one warning.
    pub fn discover(repo_root: &Path) -> Option<Self> {
        let repos_dir = crate::contracts::default_repos_dir()?;
        let declared = crate::config::Config::load(Some(repo_root))
            .map(|c| c.contracts.workspace)
            .unwrap_or_default();
        Some(Self {
            declared,
            repos_dir,
        })
    }

    /// [`Self::discover`] gated on the `[review] cross_repo` switch —
    /// the idiom both dispatch surfaces (CLI dispatch_review, MCP
    /// tool_review) used inline (TASK-086 review debt). A disabled rule
    /// C passes no inputs at all.
    pub fn discover_if_enabled(config: &crate::config::Config, repo_root: &Path) -> Option<Self> {
        config
            .review
            .cross_repo
            .then(|| Self::discover(repo_root))
            .flatten()
    }
}

/// The outcome of reviewing one diff scope.
#[derive(Debug, Clone)]
pub struct ReviewResult {
    /// The scope that was reviewed.
    pub scope: ChangeScope,
    /// The findings kept after ranking, suppression, filtering, and any
    /// cap — what the report and the verdict describe.
    pub findings: Vec<Finding>,
    /// Mechanically derived from `findings` by [`derive_verdict`].
    pub verdict: ReviewVerdict,
    /// Why each produced finding did not make the report, by reason
    /// (PRD-REV-REQ-015). Zeros included: the object is always present.
    pub drops: DropCounts,
    /// Non-fatal problems (e.g. a per-symbol blast failure) — findings the
    /// engine could not compute are never silently dropped.
    pub warnings: Vec<String>,
}

/// Per-reason counts of produced findings that did not make the report
/// (PRD-REV-REQ-015). Every finding is counted at most once, by the first
/// stage of [`rank_filter_cap`] that rejects it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DropCounts {
    /// Confidence below [`ReviewOptions::min_confidence`].
    pub below_confidence: usize,
    /// Severity below [`ReviewOptions::min_severity`].
    pub below_severity: usize,
    /// Category not in [`ReviewOptions::kinds`].
    pub out_of_category: usize,
    /// Ranked past [`ReviewOptions::max_findings`].
    pub over_cap: usize,
    /// Identity in the durable `review_suppressions` table.
    pub identity_suppressed: usize,
}

impl DropCounts {
    /// How many produced findings did not make the report, across every
    /// reason.
    pub fn total(&self) -> usize {
        self.below_confidence
            + self.below_severity
            + self.out_of_category
            + self.over_cap
            + self.identity_suppressed
    }

    /// One-line human summary of the nonzero reasons, for the text output
    /// path; `None` when nothing was dropped.
    pub fn summary_line(&self) -> Option<String> {
        if self.total() == 0 {
            return None;
        }
        let reasons = [
            ("below_confidence", self.below_confidence),
            ("below_severity", self.below_severity),
            ("out_of_category", self.out_of_category),
            ("over_cap", self.over_cap),
            ("identity_suppressed", self.identity_suppressed),
        ];
        let listed: Vec<String> = reasons
            .iter()
            .filter(|(_, n)| *n > 0)
            .map(|(name, n)| format!("{name}={n}"))
            .collect();
        Some(format!(
            "review dropped {} finding(s): {}",
            self.total(),
            listed.join(", ")
        ))
    }
}

/// Rank, suppress, filter, and cap in one pure pass (PRD-REV-REQ-015).
///
/// Order is fixed: ranking first (so a cap trims the least severe and
/// least confident), suppression before the cap (a suppressed finding
/// must not consume a cap slot), then the confidence floor, the severity
/// floor, the category filter, and the cap. Each finding is dropped by
/// exactly one reason — the first rejecting stage — so the kept list plus
/// every count always equals the input. The verdict is derived by the
/// caller from the kept list only.
pub fn rank_filter_cap(
    findings: Vec<Finding>,
    suppressed: &HashSet<String>,
    options: &ReviewOptions,
) -> (Vec<Finding>, DropCounts) {
    let mut drops = DropCounts::default();
    let mut staged = findings;
    rank_findings(&mut staged);

    let mut kept = Vec::with_capacity(staged.len());
    for finding in staged {
        if suppressed.contains(&finding.identity) {
            drops.identity_suppressed += 1;
        } else if options
            .min_confidence
            .is_some_and(|floor| finding.confidence < floor)
        {
            drops.below_confidence += 1;
        } else if options
            .min_severity
            .is_some_and(|floor| finding.severity.rank() < floor.rank())
        {
            drops.below_severity += 1;
        } else if !options.kinds.is_empty()
            && !options.kinds.iter().any(|kind| kind == &finding.kind)
        {
            drops.out_of_category += 1;
        } else {
            kept.push(finding);
        }
    }

    if let Some(cap) = options.max_findings
        && kept.len() > cap
    {
        drops.over_cap = kept.len() - cap;
        kept.truncate(cap);
    }
    (kept, drops)
}

// ---------------------------------------------------------------------------
// Finding identity (PRD-REV-REQ-013)
// ---------------------------------------------------------------------------

/// Collapse every whitespace run to a single space and trim, so re-indenting
/// or reflowing a line leaves the identity untouched while any token change
/// still alters it (AR-031). Shared with `feedback.rs`'s result identities —
/// one folding definition across review and feedback.
pub(crate) fn fold_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Stable finding identity: SHA-256 hex of
/// `rule \x1f kind \x1f file \x1f symbol \x1f fold(anchor_text)`.
///
/// The line number is structurally absent — the signature takes no line — so
/// inserting or removing lines above the finding cannot change its identity.
/// `kind` is the finding category (`breaking-change`, `coverage-gap`,
/// `cross-repo`), `file` the repo-relative path exactly as
/// [`Finding::file`] stores it, `symbol` the owning symbol's name. An
/// unresolved anchor contributes no fifth component (distinct from a
/// resolved anchor on a blank line, whose component folds to empty).
pub fn finding_identity(
    rule: &str,
    kind: &str,
    file: &str,
    symbol: &str,
    anchor_text: Option<&str>,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(rule.as_bytes());
    for part in [kind, file, symbol] {
        hasher.update([0x1f]);
        hasher.update(part.as_bytes());
    }
    if let Some(text) = anchor_text {
        hasher.update([0x1f]);
        hasher.update(fold_whitespace(text).as_bytes());
    }
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Stamp a rule-constructed finding with its identity — the ONE place
/// identities are minted for engine output. Rules push `identity:
/// String::new()`; the anchor text side is a run-review concern the rules
/// never see.
fn stamp_identity(mut finding: Finding, cs: &ChangedSymbol, anchor_text: Option<&str>) -> Finding {
    finding.identity = finding_identity(
        &finding.rule,
        &finding.kind,
        &finding.file,
        &cs.name,
        anchor_text,
    );
    finding
}

// ---------------------------------------------------------------------------
// Suppression storage (PRD-REV-REQ-014)
// ---------------------------------------------------------------------------

/// One durable suppression row: a retired finding identity plus the display
/// metadata that makes the list self-describing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Suppression {
    /// The suppressed finding's identity (the lookup key).
    pub identity: String,
    /// The finding's rule, retained for listing and bulk `--rule` removal.
    pub rule: String,
    /// The finding's file, retained for listing.
    pub file: String,
    /// Why the finding was retired, when the author said so.
    pub note: Option<String>,
    /// When the suppression was recorded (epoch seconds).
    pub created_at: i64,
}

fn now_epoch_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Record (or refresh) a suppression. Upsert keyed on identity: re-adding
/// refreshes the rule/file/note display fields instead of duplicating.
pub fn add_suppression(
    conn: &Connection,
    identity: &str,
    rule: &str,
    file: &str,
    note: Option<&str>,
) -> Result<()> {
    conn.execute(
        "INSERT INTO review_suppressions (identity, rule, file, note, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5) \
         ON CONFLICT(identity) DO UPDATE SET \
         rule = excluded.rule, file = excluded.file, note = excluded.note",
        rusqlite::params![identity, rule, file, note, now_epoch_secs()],
    )?;
    Ok(())
}

/// List suppressions, newest first, ordered by `created_at` then `identity`
/// (within one second, insertion order is arbitrary — identity makes the
/// listing deterministic). `rule` filters to one rule family.
pub fn list_suppressions(conn: &Connection, rule: Option<&str>) -> Result<Vec<Suppression>> {
    let mut stmt = conn.prepare(
        "SELECT identity, rule, file, note, created_at FROM review_suppressions \
         WHERE (?1 IS NULL OR rule = ?1) \
         ORDER BY created_at DESC, identity",
    )?;
    let rows = stmt.query_map(rusqlite::params![rule], |row| {
        Ok(Suppression {
            identity: row.get(0)?,
            rule: row.get(1)?,
            file: row.get(2)?,
            note: row.get(3)?,
            created_at: row.get(4)?,
        })
    })?;
    Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
}

/// Remove suppressions by identity and/or rule (whichever is given; both is
/// the union) and return how many rows went.
pub fn remove_suppressions(
    conn: &Connection,
    identities: &[String],
    rule: Option<&str>,
) -> Result<usize> {
    if identities.is_empty() && rule.is_none() {
        return Ok(0);
    }
    let mut conditions: Vec<String> = Vec::new();
    let mut params: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
    if !identities.is_empty() {
        let placeholders = identities
            .iter()
            .map(|id| {
                params.push(Box::new(id.clone()));
                format!("?{}", params.len())
            })
            .collect::<Vec<_>>()
            .join(", ");
        conditions.push(format!("identity IN ({placeholders})"));
    }
    if let Some(rule) = rule {
        params.push(Box::new(rule.to_string()));
        conditions.push(format!("rule = ?{}", params.len()));
    }
    let sql = format!(
        "DELETE FROM review_suppressions WHERE {}",
        conditions.join(" OR ")
    );
    let refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(|p| p.as_ref()).collect();
    Ok(conn.execute(sql.as_str(), refs.as_slice())?)
}

/// The identities currently suppressed — the set [`run_review`] consults
/// before keeping a finding.
pub fn suppressed_identities(conn: &Connection) -> Result<HashSet<String>> {
    let mut stmt = conn.prepare("SELECT identity FROM review_suppressions")?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
    Ok(rows.collect::<std::result::Result<HashSet<_>, _>>()?)
}

// ---------------------------------------------------------------------------
// Rule family A — breaking change (PRD-REV-REQ-006)
// ---------------------------------------------------------------------------

/// Fixed confidence for coverage-gap findings (rule B). Provisional value;
/// OQ-013 owns calibration (PRD-REV-REQ-015).
pub const COVERAGE_GAP_CONFIDENCE: f64 = 0.8;

/// Fixed confidence for cross-repo findings (rule C). Provisional value;
/// OQ-013 owns calibration (PRD-REV-REQ-015).
pub const CROSS_REPO_CONFIDENCE: f64 = 0.8;

/// Rank findings worst-first: severity desc, then confidence desc, then
/// file, line, rule (PRD-REV-REQ-015). The rank order decides which
/// findings a cap trims — the least severe and least confident go first.
pub fn rank_findings(findings: &mut [Finding]) {
    findings.sort_by(|a, b| {
        b.severity
            .rank()
            .cmp(&a.severity.rank())
            .then_with(|| b.confidence.total_cmp(&a.confidence))
            .then_with(|| a.file.cmp(&b.file))
            .then_with(|| a.line.cmp(&b.line))
            .then_with(|| a.rule.cmp(&b.rule))
    });
}

/// Format the caller-name list: up to three names, then ` (+k more)`.
fn format_caller_names(names: &[&str]) -> String {
    let listed = names.iter().take(3).copied().collect::<Vec<_>>().join(", ");
    if names.len() > 3 {
        format!("{listed} (+{} more)", names.len() - 3)
    } else {
        listed
    }
}

/// Rule A: a removed or signature-changed symbol with live indexed direct
/// callers blocks. Body-only modifications are never candidates.
///
/// Direct callers are the WillBreak tier (depth 1) of the context
/// [`analyze_blast`] call — never re-derived here. A caller that is itself
/// Removed in the same diff is dead code being deleted along with its
/// helper, so it is filtered out; signature-changed callers stay.
fn rule_breaking_change(
    cs: &ChangedSymbol,
    context: &BlastAnalysis,
    removed: &HashSet<(String, crate::types::SymbolKind, String)>,
    line: Option<usize>,
    anchor_method: AnchorMethod,
) -> Option<Finding> {
    let surviving: Vec<&BlastAffectedSymbol> = context
        .tiers
        .iter()
        .find(|t| t.severity == BlastSeverity::WillBreak)
        .map(|t| {
            t.symbols
                .iter()
                .filter(|s| !removed.contains(&(s.name.clone(), s.kind, s.file.clone())))
                .collect()
        })
        .unwrap_or_default();
    if surviving.is_empty() {
        return None;
    }

    let names: Vec<&str> = surviving.iter().map(|s| s.name.as_str()).collect();
    // REQ-015: data-derived confidence — the strongest surviving caller
    // edge. (A caller deleted in the same diff never reaches `surviving`.)
    let confidence = surviving
        .iter()
        .map(|s| s.confidence)
        .fold(f64::NEG_INFINITY, f64::max);
    let (rule, message) = if cs.change_type == crate::types::ChangeType::Removed {
        (
            "breaking-change/removed-symbol-with-callers",
            format!(
                "removed {} `{}` still has {} indexed caller(s): {}",
                cs.kind,
                cs.name,
                names.len(),
                format_caller_names(&names)
            ),
        )
    } else {
        (
            "breaking-change/signature-changed-with-callers",
            format!(
                "signature of {} `{}` changed but {} indexed caller(s) remain: {}",
                cs.kind,
                cs.name,
                names.len(),
                format_caller_names(&names)
            ),
        )
    };

    Some(Finding {
        file: cs.file.clone(),
        line,
        anchor_method,
        severity: FindingSeverity::Blocking,
        confidence,
        kind: "breaking-change".into(),
        rule: rule.into(),
        message,
        identity: String::new(),
        related: surviving.iter().map(|&s| SymbolRef::from(s)).collect(),
    })
}

// ---------------------------------------------------------------------------
// Rule family B — coverage gap (PRD-REV-REQ-007)
// ---------------------------------------------------------------------------

/// Rule B: an added/modified non-test symbol whose blast radius contains no
/// test-file symbols is untested — a warning, never a block.
///
/// The coverage QUESTION is asked over the with-tests blast (the only way to
/// see test callers at all), but the finding's `related` context is the
/// tests-excluded context analysis — the canonical `wonk blast` radius.
fn rule_coverage_gap(
    cs: &ChangedSymbol,
    with_tests: &BlastAnalysis,
    context: &BlastAnalysis,
    line: Option<usize>,
    anchor_method: AnchorMethod,
) -> Option<Finding> {
    let radius: Vec<&BlastAffectedSymbol> = with_tests
        .tiers
        .iter()
        .flat_map(|t| t.symbols.iter())
        .collect();
    // TASK-094 keep: review semantics — test symbols sit outside review
    // scope; this is not a ranking demotion.
    if radius
        .iter()
        .any(|s| crate::ranker::is_test_file(Path::new(&s.file)))
    {
        return None;
    }

    let rule = "coverage-gap/no-test-in-blast-radius";
    let message = if radius.is_empty() {
        format!(
            "{} `{}` changed but no test file appears in its blast radius (no affected symbols indexed)",
            cs.kind, cs.name
        )
    } else {
        format!(
            "{} `{}` changed but no test file appears in its blast radius ({} affected symbol(s), none in tests)",
            cs.kind,
            cs.name,
            radius.len()
        )
    };

    Some(Finding {
        file: cs.file.clone(),
        line,
        anchor_method,
        severity: FindingSeverity::Warning,
        confidence: COVERAGE_GAP_CONFIDENCE,
        kind: "coverage-gap".into(),
        rule: rule.into(),
        message,
        identity: String::new(),
        related: context
            .tiers
            .iter()
            .flat_map(|t| t.symbols.iter())
            .map(SymbolRef::from)
            .collect(),
    })
}

// ---------------------------------------------------------------------------
// Rule family C — cross-repo contract impact (PRD-REV-REQ-010)
// ---------------------------------------------------------------------------

/// Rule C: a changed symbol that PROVIDES a contract consumed by another
/// indexed repo warns, naming the consuming repo.
///
/// All three change types are candidates — including `Removed`: the
/// base-state index still holds the removed provider's contract rows, and
/// its external consumers are invisible to rule A (whose callers are the
/// in-repo WillBreak tier of `analyze_blast`), so without rule C the
/// highest-impact cross-repo change would be silent. A Removed provider
/// with BOTH in-repo callers and external consumers can produce both
/// findings — the rules are independent, matching the existing A+B
/// coexistence, and rule A remains the blocking path. Body-only
/// modifications DO count: sibling repos depend on behavior (routes
/// handled, messages emitted), not on signatures. `related` folds consumer
/// sites exactly like blast's cross-repo tier (`append_cross_repo_tier`),
/// so review and `wonk blast` cannot disagree.
fn rule_cross_repo(
    cs: &ChangedSymbol,
    provider_ids: &[String],
    resolution: &crate::contracts::WorkspaceResolution,
    line: Option<usize>,
    anchor_method: AnchorMethod,
) -> Option<Finding> {
    let own = &resolution.scope.repo_name;
    let consumers: Vec<&crate::contracts::CrossRepoLink> = resolution
        .links
        .iter()
        .filter(|l| l.provider.repo == *own && provider_ids.contains(&l.provider.canonical_id))
        .collect();
    if consumers.is_empty() {
        return None;
    }

    // One SymbolRef per consumer site, folded identically to
    // append_cross_repo_tier, deduplicated on the site.
    let mut seen: HashSet<(String, String, usize)> = HashSet::new();
    let related: Vec<SymbolRef> = consumers
        .iter()
        .filter(|l| {
            seen.insert((
                l.consumer.repo.clone(),
                l.consumer.file.clone(),
                l.consumer.line,
            ))
        })
        .map(|l| {
            let c = &l.consumer;
            SymbolRef {
                name: c.symbol.clone().unwrap_or_else(|| c.canonical_id.clone()),
                kind: crate::types::SymbolKind::Function,
                file: format!("{}:{}", c.repo, c.file),
                line: c.line,
            }
        })
        .collect();

    let mut repos: Vec<&str> = Vec::new();
    for l in &consumers {
        let name = l.consumer.repo.as_str();
        if !repos.contains(&name) {
            repos.push(name);
        }
    }

    let rule = "cross-repo/changed-provider-with-external-consumers";
    let message = if cs.change_type == crate::types::ChangeType::Removed {
        format!(
            "removed {} `{}` provided contract(s) {} consumed by {} other repo(s): {}",
            cs.kind,
            cs.name,
            provider_ids.join(", "),
            repos.len(),
            format_caller_names(&repos)
        )
    } else {
        format!(
            "{} `{}` changed but provides contract(s) {} consumed by {} other repo(s): {}",
            cs.kind,
            cs.name,
            provider_ids.join(", "),
            repos.len(),
            format_caller_names(&repos)
        )
    };

    Some(Finding {
        file: cs.file.clone(),
        line,
        anchor_method,
        severity: FindingSeverity::Warning,
        confidence: CROSS_REPO_CONFIDENCE,
        kind: "cross-repo".into(),
        rule: rule.into(),
        message,
        identity: String::new(),
        related,
    })
}

/// The one workspace join a review run may perform (rule C): list the
/// repo's full contract row set, then resolve links over the registry in
/// `inputs`. Same two steps `wonk contracts --links` runs, so the surfaces
/// share one resolution semantics.
fn resolve_workspace_once(
    conn: &Connection,
    repo_root: &Path,
    inputs: &CrossRepoInputs,
) -> Result<crate::contracts::WorkspaceResolution> {
    let rows = crate::contracts::list_contracts(conn, &crate::contracts::ContractQuery::default())?;
    crate::contracts::resolve_workspace(repo_root, &rows, &inputs.declared, &inputs.repos_dir)
}

// ---------------------------------------------------------------------------
// run_review
// ---------------------------------------------------------------------------

/// Review one diff scope: detect changed symbols, run the enabled rule
/// families over them, and derive the verdict.
///
/// Impact context comes exclusively from [`blast::analyze_blast`] — review
/// records blast's own output and never computes impact itself, so `wonk
/// review` and `wonk blast` cannot disagree for the same symbol. Per-symbol
/// blast failures degrade to a warning and skip that symbol (fail-soft).
///
/// Rule C composition: `cross_repo` carries the explicit registry inputs
/// (see [`CrossRepoInputs`]); the workspace is resolved AT MOST ONCE per
/// run — lazily, at the first rule-C candidate that owns provider
/// contracts — and every candidate filters that one shared resolution.
/// That is both the perf bound (one registry join per run, never per
/// symbol) and the guarantee that rule C can never disagree with the
/// `wonk contracts --links` resolution of the same run. A resolution
/// failure warns once and disables rule C for the run; rules A/B are
/// unaffected.
pub fn run_review(
    conn: &Connection,
    scope: &ChangeScope,
    repo_root: &Path,
    options: &ReviewOptions,
    cross_repo: Option<&CrossRepoInputs>,
) -> Result<ReviewResult> {
    let detail = impact::detect_changes_detail(conn, scope, repo_root)?;
    let mut warnings = Vec::new();
    let mut findings = Vec::new();

    // Durable suppressions (PRD-REV-REQ-014): consulted before a finding is
    // kept. The ensure covers pre-TASK-089 indexes (a no-op otherwise).
    crate::db::ensure_review_suppressions_table(conn)?;
    let suppressed = suppressed_identities(conn)?;

    // Callers removed in this same diff are dead code, not breakage.
    // Keyed on (name, kind, FILE): a live caller that merely shares
    // name+kind with an unrelated removed symbol (common method names —
    // run/apply/new — since SymbolKind encodes no receiver) must not be
    // dropped as dead code; both sides carry old-side indexed paths
    // (TASK-085 review debt).
    let removed: HashSet<(String, crate::types::SymbolKind, String)> = detail
        .analysis
        .changed_symbols
        .iter()
        .filter(|c| c.change_type == crate::types::ChangeType::Removed)
        .map(|c| (c.name.clone(), c.kind, c.file.clone()))
        .collect();

    // Per-file cache of current-file symbols for tier-3 re-resolution.
    let mut current_cache: HashMap<String, Option<Vec<Symbol>>> = HashMap::new();
    // Per-file cache of current-file LINES — the tier 1/3 anchor-text side.
    let mut current_lines_cache: HashMap<String, Option<Vec<String>>> = HashMap::new();

    // Outer = attempted (None until the first rule-C candidate with
    // provider contracts); inner = the resolution, None when it failed.
    let mut cross_repo_resolution: Option<Option<crate::contracts::WorkspaceResolution>> = None;
    let mut cross_repo_inputs_warning = false;

    for cs in &detail.analysis.changed_symbols {
        let rule_a_candidate = options.breaking_change
            && (cs.change_type == crate::types::ChangeType::Removed
                || (cs.change_type == crate::types::ChangeType::Modified
                    && detail
                        .signature_changed
                        .contains(&(cs.name.clone(), cs.kind))));
        // TASK-094 keep: review semantics — test symbols sit outside review
        // scope; this is not a ranking demotion.
        let rule_b_candidate = options.coverage_gap
            && matches!(
                cs.change_type,
                crate::types::ChangeType::Added | crate::types::ChangeType::Modified
            )
            && !crate::ranker::is_test_file(Path::new(&cs.file));
        let rule_c_candidate = options.cross_repo
            && matches!(
                cs.change_type,
                crate::types::ChangeType::Added
                    | crate::types::ChangeType::Modified
                    | crate::types::ChangeType::Removed
            )
            && !crate::ranker::is_test_file(Path::new(&cs.file));
        if !rule_a_candidate && !rule_b_candidate && !rule_c_candidate {
            continue;
        }

        // Context blast (rules A/B): byte-identical options to `wonk blast`
        // defaults, so the recorded impact can never disagree with it.
        // Rule-C-only candidates skip it entirely — their impact evidence
        // is contract rows, not the call graph.
        let context_options = BlastOptions {
            depth: options.depth,
            direction: BlastDirection::Upstream,
            include_tests: false,
            min_confidence: None,
            use_reach: options.reach_enabled,
        };
        let context = if rule_a_candidate || rule_b_candidate {
            match blast::analyze_blast(conn, &cs.name, &context_options) {
                Ok(analysis) => Some(analysis),
                Err(e) => {
                    warnings.push(format!(
                        "skipping {} `{}` in {}: blast failed: {e}",
                        cs.kind, cs.name, cs.file
                    ));
                    continue;
                }
            }
        } else {
            None
        };

        if !current_cache.contains_key(&cs.file) {
            // A deleted file has nothing to re-resolve; Removed symbols
            // anchor from the old side and never need this.
            let parsed = impact::parse_current_symbols(&cs.file, repo_root).ok();
            current_cache.insert(cs.file.clone(), parsed);
        }
        let current_symbols = current_cache.get(&cs.file).and_then(|opt| opt.as_deref());
        if !current_lines_cache.contains_key(&cs.file) {
            let lines = std::fs::read_to_string(repo_root.join(&cs.file))
                .ok()
                .map(|s| s.lines().map(str::to_string).collect::<Vec<_>>());
            current_lines_cache.insert(cs.file.clone(), lines);
        }
        let current_lines = current_lines_cache
            .get(&cs.file)
            .and_then(|opt| opt.as_deref());
        let (line, anchor_method) = resolve_anchor(cs, detail.hunks.get(&cs.file), current_symbols);
        // Anchor text is resolved once per symbol: the side the anchor
        // resolved against, feeding the identity stamped at the push seam.
        let anchor_text = anchored_line_text(
            anchor_method,
            line,
            detail.hunks.get(&cs.file),
            current_lines,
        );

        if rule_a_candidate
            && let Some(ref context) = context
            && let Some(finding) = rule_breaking_change(cs, context, &removed, line, anchor_method)
        {
            findings.push(stamp_identity(finding, cs, anchor_text.as_deref()));
        }

        if rule_b_candidate && let Some(ref context) = context {
            // Same options but include_tests: reach routing correctly
            // declines this shape, so the shared live BFS answers it.
            let with_tests = blast::analyze_blast(
                conn,
                &cs.name,
                &BlastOptions {
                    include_tests: true,
                    ..context_options.clone()
                },
            );
            match with_tests {
                Ok(with_tests) => {
                    if let Some(finding) =
                        rule_coverage_gap(cs, &with_tests, context, line, anchor_method)
                    {
                        findings.push(stamp_identity(finding, cs, anchor_text.as_deref()));
                    }
                }
                Err(e) => warnings.push(format!(
                    "coverage check skipped for {} `{}` in {}: blast failed: {e}",
                    cs.kind, cs.name, cs.file
                )),
            }
        }

        if rule_c_candidate {
            match cross_repo {
                Some(inputs) => match blast::provider_contract_ids(conn, &cs.name) {
                    Ok(ids) if ids.is_empty() => {}
                    Ok(ids) => {
                        if cross_repo_resolution.is_none() {
                            let attempted = match resolve_workspace_once(conn, repo_root, inputs) {
                                Ok(resolution) => Some(resolution),
                                Err(e) => {
                                    warnings.push(format!(
                                            "cross-repo impact skipped: workspace resolution failed: {e}"
                                        ));
                                    None
                                }
                            };
                            cross_repo_resolution = Some(attempted);
                        }
                        if let Some(Some(resolution)) = &cross_repo_resolution
                            && let Some(finding) =
                                rule_cross_repo(cs, &ids, resolution, line, anchor_method)
                        {
                            findings.push(stamp_identity(finding, cs, anchor_text.as_deref()));
                        }
                    }
                    Err(e) => warnings.push(format!(
                        "cross-repo check skipped for {} `{}` in {}: {e}",
                        cs.kind, cs.name, cs.file
                    )),
                },
                None if !cross_repo_inputs_warning => {
                    cross_repo_inputs_warning = true;
                    warnings.push(
                        "cross-repo impact skipped: no cross-repo inputs available \
                         (no home directory found)"
                            .into(),
                    );
                }
                None => {}
            }
        }
    }

    // One pure pass decides what the report contains (PRD-REV-REQ-015):
    // suppressed identities never reach the report or the verdict — a
    // retired false positive is not a finding of this run — and the
    // verdict derives from what is kept.
    let (findings, drops) = rank_filter_cap(findings, &suppressed, options);

    let verdict = derive_verdict(&findings);

    Ok(ReviewResult {
        scope: scope.clone(),
        findings,
        verdict,
        drops,
        warnings,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{
        AnchorMethod, ChangeType, Finding, FindingSeverity, ReviewVerdict, SymbolKind,
    };

    // -- derive_verdict table tests --------------------------------------------

    #[test]
    fn derive_verdict_empty_is_approve() {
        assert_eq!(derive_verdict(&[]), ReviewVerdict::Approve);
    }

    #[test]
    fn derive_verdict_notes_only_is_approve() {
        let findings = vec![
            finding(FindingSeverity::Note),
            finding(FindingSeverity::Note),
        ];
        assert_eq!(derive_verdict(&findings), ReviewVerdict::Approve);
    }

    #[test]
    fn derive_verdict_any_warning_is_review() {
        let findings = vec![
            finding(FindingSeverity::Note),
            finding(FindingSeverity::Warning),
        ];
        assert_eq!(derive_verdict(&findings), ReviewVerdict::Review);
    }

    #[test]
    fn derive_verdict_any_blocking_is_block() {
        let findings = vec![
            finding(FindingSeverity::Warning),
            finding(FindingSeverity::Blocking),
            finding(FindingSeverity::Note),
        ];
        assert_eq!(derive_verdict(&findings), ReviewVerdict::Block);
    }

    fn finding(severity: FindingSeverity) -> Finding {
        Finding {
            file: "src/lib.rs".into(),
            line: Some(1),
            anchor_method: crate::types::AnchorMethod::PostChangeFile,
            severity,
            confidence: 0.5,
            kind: "test".into(),
            rule: "test/rule".into(),
            message: "msg".into(),
            identity: "id".into(),
            related: vec![],
        }
    }

    // -- finding identity (TASK-089, PRD-REV-REQ-013) ---------------------------

    #[test]
    fn identity_is_64_hex_characters() {
        let id = finding_identity(
            "breaking-change/removed-symbol-with-callers",
            "breaking-change",
            "src/lib.rs",
            "used",
            Some("pub fn used() {}"),
        );
        assert_eq!(id.len(), 64);
        assert!(
            id.bytes().all(|b| b.is_ascii_hexdigit()),
            "identity must be lowercase hex, got {id}"
        );
    }

    #[test]
    fn identity_survives_whitespace_only_reformat() {
        // Re-indenting or reflowing the anchored line changes no token, so
        // the folded text — and the identity — is unchanged (REQ-013).
        let compact = finding_identity("r", "k", "src/lib.rs", "f", Some("fn f(x: i32) -> i32 {"));
        let reindented = finding_identity(
            "r",
            "k",
            "src/lib.rs",
            "f",
            Some("  fn  f(x: i32)\t->  i32  {\n"),
        );
        assert_eq!(compact, reindented);
    }

    #[test]
    fn identity_changes_when_any_component_changes() {
        let base = finding_identity("r", "k", "src/lib.rs", "f", Some("fn f() {}"));
        assert_ne!(
            finding_identity("other", "k", "src/lib.rs", "f", Some("fn f() {}")),
            base,
            "rule is signed"
        );
        assert_ne!(
            finding_identity("r", "coverage-gap", "src/lib.rs", "f", Some("fn f() {}")),
            base,
            "kind is signed"
        );
        assert_ne!(
            finding_identity("r", "k", "src/other.rs", "f", Some("fn f() {}")),
            base,
            "file is signed"
        );
        assert_ne!(
            finding_identity("r", "k", "src/lib.rs", "g", Some("fn f() {}")),
            base,
            "symbol is signed"
        );
        assert_ne!(
            finding_identity("r", "k", "src/lib.rs", "f", Some("fn f(x: u64) {}")),
            base,
            "any token change on the anchored line is signed (AR-031)"
        );
    }

    #[test]
    fn identity_unresolved_anchor_is_deterministic_and_distinct_from_blank_line() {
        let unresolved = finding_identity("r", "k", "src/lib.rs", "f", None);
        assert_eq!(
            unresolved,
            finding_identity("r", "k", "src/lib.rs", "f", None),
            "unresolved anchors hash deterministically"
        );
        // A resolved anchor on a blank line folds to an empty component;
        // an unresolved anchor contributes no component at all — the two
        // must never collide.
        assert_ne!(
            unresolved,
            finding_identity("r", "k", "src/lib.rs", "f", Some("")),
            "unresolved must not collide with a resolved blank line"
        );
        // Unresolved identities still distinguish rule/file/symbol.
        assert_ne!(
            unresolved,
            finding_identity("r", "k", "src/lib.rs", "g", None)
        );
    }

    // -- confidence + ranking (TASK-089, PRD-REV-REQ-015) -----------------------

    fn ranked(
        severity: FindingSeverity,
        confidence: f64,
        file: &str,
        line: Option<usize>,
        rule: &str,
    ) -> Finding {
        Finding {
            file: file.into(),
            line,
            anchor_method: AnchorMethod::PostChangeFile,
            severity,
            kind: "test".into(),
            rule: rule.into(),
            message: "m".into(),
            identity: format!("id-{rule}"),
            confidence,
            related: vec![],
        }
    }

    #[test]
    fn coverage_gap_and_cross_repo_confidence_consts_are_calibrated() {
        // The fixed confidences are provisional by design (OQ-013 owns
        // calibration); pinning the values here makes a later re-tuning a
        // visible, reviewed change.
        assert_eq!(COVERAGE_GAP_CONFIDENCE, 0.8);
        assert_eq!(CROSS_REPO_CONFIDENCE, 0.8);
        assert!(
            (0.0..=1.0).contains(&COVERAGE_GAP_CONFIDENCE)
                && (0.0..=1.0).contains(&CROSS_REPO_CONFIDENCE),
            "confidence is a [0, 1] quantity"
        );
    }

    #[test]
    fn ranking_is_severity_desc_then_confidence_desc_then_file_line_rule() {
        let mut findings = vec![
            ranked(FindingSeverity::Note, 0.99, "src/a.rs", Some(1), "r-note"),
            ranked(FindingSeverity::Warning, 0.5, "src/a.rs", Some(9), "r-a9"),
            ranked(FindingSeverity::Warning, 0.9, "src/b.rs", Some(2), "r-conf"),
            ranked(FindingSeverity::Warning, 0.5, "src/a.rs", Some(2), "r-a2b"),
            ranked(FindingSeverity::Warning, 0.5, "src/a.rs", Some(2), "r-a2a"),
            ranked(
                FindingSeverity::Blocking,
                0.1,
                "src/z.rs",
                Some(1),
                "r-block",
            ),
        ];
        rank_findings(&mut findings);
        let rules: Vec<&str> = findings.iter().map(|f| f.rule.as_str()).collect();
        assert_eq!(
            rules,
            vec!["r-block", "r-conf", "r-a2a", "r-a2b", "r-a9", "r-note"],
            "severity desc, then confidence desc, then file, line, rule"
        );
    }

    #[test]
    fn ranking_unresolved_lines_sort_first_within_their_tier() {
        let mut findings = vec![
            ranked(
                FindingSeverity::Warning,
                0.5,
                "src/a.rs",
                Some(4),
                "r-lined",
            ),
            ranked(
                FindingSeverity::Warning,
                0.5,
                "src/a.rs",
                None,
                "r-unanchored",
            ),
        ];
        rank_findings(&mut findings);
        assert_eq!(findings[0].rule, "r-unanchored");
    }

    // -- filter + cap pipeline (TASK-089, PRD-REV-REQ-015) ----------------------

    fn pipelined(severity: FindingSeverity, confidence: f64, kind: &str, rule: &str) -> Finding {
        Finding {
            kind: kind.into(),
            ..ranked(severity, confidence, "src/a.rs", Some(1), rule)
        }
    }

    #[test]
    fn cap_trims_the_least_severe_and_counts_over_cap() {
        let findings = vec![
            pipelined(FindingSeverity::Note, 0.9, "test", "r-note"),
            pipelined(FindingSeverity::Warning, 0.5, "test", "r-warn-lo"),
            pipelined(FindingSeverity::Warning, 0.9, "test", "r-warn-hi"),
            pipelined(FindingSeverity::Blocking, 0.1, "test", "r-block"),
        ];
        let options = ReviewOptions {
            max_findings: Some(2),
            ..ReviewOptions::default()
        };
        let (kept, drops) = rank_filter_cap(findings, &HashSet::new(), &options);
        let rules: Vec<&str> = kept.iter().map(|f| f.rule.as_str()).collect();
        assert_eq!(rules, vec!["r-block", "r-warn-hi"], "cap keeps the worst");
        assert_eq!(drops.over_cap, 2);
        assert_eq!(
            drops,
            DropCounts {
                over_cap: 2,
                ..DropCounts::default()
            }
        );
    }

    #[test]
    fn each_stage_counts_only_its_own_drops() {
        // One finding per stage, each dropped by exactly the first stage
        // that rejects it — plus one that rejects on none.
        let findings = vec![
            pipelined(FindingSeverity::Warning, 0.3, "test", "r-conf"),
            pipelined(FindingSeverity::Note, 0.9, "test", "r-sev"),
            pipelined(FindingSeverity::Warning, 0.9, "other", "r-kind"),
            pipelined(FindingSeverity::Blocking, 0.9, "test", "r-kept"),
        ];
        let options = ReviewOptions {
            min_confidence: Some(0.5),
            min_severity: Some(FindingSeverity::Warning),
            kinds: vec!["test".into()],
            ..ReviewOptions::default()
        };
        let (kept, drops) = rank_filter_cap(findings, &HashSet::new(), &options);
        assert_eq!(
            kept.iter().map(|f| f.rule.as_str()).collect::<Vec<_>>(),
            vec!["r-kept"]
        );
        assert_eq!(drops.below_confidence, 1);
        assert_eq!(drops.below_severity, 1);
        assert_eq!(drops.out_of_category, 1);
        assert_eq!(drops.over_cap, 0);
        assert_eq!(drops.identity_suppressed, 0);
    }

    #[test]
    fn suppressed_findings_never_count_as_any_other_drop() {
        // The suppressed finding is also below every filter floor — the
        // first rejecting stage is suppression, so it must be counted
        // there and only there.
        let findings = vec![
            pipelined(FindingSeverity::Note, 0.1, "other", "r-sup"),
            pipelined(FindingSeverity::Blocking, 0.9, "test", "r-kept"),
        ];
        let suppressed: HashSet<String> = ["id-r-sup"].into_iter().map(str::to_string).collect();
        let options = ReviewOptions {
            min_confidence: Some(0.5),
            min_severity: Some(FindingSeverity::Warning),
            kinds: vec!["test".into()],
            ..ReviewOptions::default()
        };
        let (kept, drops) = rank_filter_cap(findings, &suppressed, &options);
        assert_eq!(
            kept.iter().map(|f| f.rule.as_str()).collect::<Vec<_>>(),
            vec!["r-kept"]
        );
        assert_eq!(drops.identity_suppressed, 1);
        assert_eq!(drops.below_confidence, 0);
        assert_eq!(drops.below_severity, 0);
        assert_eq!(drops.out_of_category, 0);
    }

    #[test]
    fn kept_plus_every_drop_reason_equals_produced() {
        let findings = vec![
            pipelined(FindingSeverity::Note, 0.2, "other", "r-a"),
            pipelined(FindingSeverity::Warning, 0.5, "test", "r-b"),
            pipelined(FindingSeverity::Blocking, 0.95, "test", "r-c"),
            pipelined(FindingSeverity::Warning, 0.9, "test", "r-d"),
            pipelined(FindingSeverity::Warning, 0.4, "test", "r-e"),
        ];
        let suppressed: HashSet<String> = ["id-r-c"].into_iter().map(str::to_string).collect();
        let options = ReviewOptions {
            min_confidence: Some(0.45),
            min_severity: Some(FindingSeverity::Warning),
            kinds: vec!["test".into()],
            max_findings: Some(1),
            ..ReviewOptions::default()
        };
        let (kept, drops) = rank_filter_cap(findings, &suppressed, &options);
        let total = kept.len()
            + drops.identity_suppressed
            + drops.below_confidence
            + drops.below_severity
            + drops.out_of_category
            + drops.over_cap;
        assert_eq!(total, 5, "every finding is kept or dropped exactly once");
    }

    #[test]
    fn all_filters_off_keeps_everything_with_zero_drops() {
        let findings = vec![
            pipelined(FindingSeverity::Note, 0.1, "other", "r-a"),
            pipelined(FindingSeverity::Blocking, 0.9, "test", "r-b"),
        ];
        let expected: Vec<String> = vec!["r-b".into(), "r-a".into()];
        let (kept, drops) =
            rank_filter_cap(findings.clone(), &HashSet::new(), &ReviewOptions::default());
        assert_eq!(
            kept.iter().map(|f| f.rule.clone()).collect::<Vec<_>>(),
            expected,
            "defaults are today's behavior: rank only"
        );
        assert_eq!(drops, DropCounts::default());
    }

    #[test]
    fn confidence_floor_is_inclusive() {
        let findings = vec![pipelined(FindingSeverity::Warning, 0.5, "test", "r-at")];
        let options = ReviewOptions {
            min_confidence: Some(0.5),
            ..ReviewOptions::default()
        };
        let (kept, drops) = rank_filter_cap(findings, &HashSet::new(), &options);
        assert_eq!(kept.len(), 1, "a finding AT the floor survives");
        assert_eq!(drops.below_confidence, 0);
    }

    #[test]
    fn drop_counts_summary_line_lists_only_nonzero_reasons() {
        assert_eq!(DropCounts::default().summary_line(), None);
        assert_eq!(
            DropCounts {
                below_confidence: 2,
                over_cap: 1,
                ..DropCounts::default()
            }
            .summary_line()
            .as_deref(),
            Some("review dropped 3 finding(s): below_confidence=2, over_cap=1")
        );
    }

    #[test]
    fn severity_floor_keeps_the_named_tier_and_above() {
        let findings = vec![
            pipelined(FindingSeverity::Blocking, 0.9, "test", "r-block"),
            pipelined(FindingSeverity::Warning, 0.9, "test", "r-warn"),
            pipelined(FindingSeverity::Note, 0.9, "test", "r-note"),
        ];
        let options = ReviewOptions {
            min_severity: Some(FindingSeverity::Warning),
            ..ReviewOptions::default()
        };
        let (kept, drops) = rank_filter_cap(findings, &HashSet::new(), &options);
        assert_eq!(
            kept.iter().map(|f| f.rule.as_str()).collect::<Vec<_>>(),
            vec!["r-block", "r-warn"]
        );
        assert_eq!(drops.below_severity, 1);
    }

    // -- resolve_anchor table tests ---------------------------------------------

    fn changed(change_type: ChangeType, line: usize) -> ChangedSymbol {
        ChangedSymbol {
            name: "f".into(),
            kind: SymbolKind::Function,
            file: "src/lib.rs".into(),
            line,
            change_type,
        }
    }

    fn hunks(new: Vec<(usize, usize)>, removed: Vec<(usize, usize)>) -> FileDiffHunks {
        FileDiffHunks {
            new_ranges: new,
            removed_ranges: removed,
            removed_lines: HashMap::new(),
        }
    }

    fn current_symbol(line: usize) -> Symbol {
        Symbol {
            name: "f".into(),
            kind: SymbolKind::Function,
            file: "src/lib.rs".into(),
            line,
            col: 0,
            end_line: Some(line + 2),
            scope: None,
            signature: "fn f()".into(),
            language: "rust".into(),
            doc_comment: None,
        }
    }

    #[test]
    fn resolve_anchor_modified_inside_new_hunk_is_new_side_hunk() {
        let cs = changed(ChangeType::Modified, 10);
        let h = hunks(vec![(8, 12)], vec![(8, 10)]);
        let cur = vec![current_symbol(10)];
        assert_eq!(
            resolve_anchor(&cs, Some(&h), Some(&cur)),
            (Some(10), AnchorMethod::NewSideHunk)
        );
    }

    #[test]
    fn resolve_anchor_modified_outside_hunk_re_resolves_to_post_change_file() {
        // Body-only hunk-overlap Modified: the current symbol line (10) is
        // not inside the new-side hunk, so tier 3 pins the re-resolved
        // current line rather than the stale indexed line.
        let cs = changed(ChangeType::Modified, 3);
        let h = hunks(vec![(20, 25)], vec![]);
        let cur = vec![current_symbol(10)];
        assert_eq!(
            resolve_anchor(&cs, Some(&h), Some(&cur)),
            (Some(10), AnchorMethod::PostChangeFile)
        );
    }

    #[test]
    fn resolve_anchor_no_hunks_still_anchors_to_post_change_file() {
        let cs = changed(ChangeType::Modified, 3);
        let cur = vec![current_symbol(7)];
        assert_eq!(
            resolve_anchor(&cs, None, Some(&cur)),
            (Some(7), AnchorMethod::PostChangeFile)
        );
    }

    #[test]
    fn resolve_anchor_modified_without_current_symbols_is_unresolved() {
        let cs = changed(ChangeType::Modified, 3);
        let h = hunks(vec![(3, 3)], vec![]);
        assert_eq!(
            resolve_anchor(&cs, Some(&h), None),
            (None, AnchorMethod::Unresolved)
        );
        // Also when the current file no longer contains the symbol.
        let cur: Vec<Symbol> = vec![];
        assert_eq!(
            resolve_anchor(&cs, Some(&h), Some(&cur)),
            (None, AnchorMethod::Unresolved)
        );
    }

    #[test]
    fn resolve_anchor_added_symbol_uses_new_side_hunk() {
        let cs = changed(ChangeType::Added, 4);
        let h = hunks(vec![(4, 6)], vec![]);
        let cur = vec![current_symbol(4)];
        assert_eq!(
            resolve_anchor(&cs, Some(&h), Some(&cur)),
            (Some(4), AnchorMethod::NewSideHunk)
        );
    }

    #[test]
    fn resolve_anchor_removed_covered_by_removed_range_is_old_side_line() {
        // Indexed line == old-side line because the index reflects the base
        // state; anchor to where the symbol WAS.
        let cs = changed(ChangeType::Removed, 5);
        let h = hunks(vec![], vec![(3, 8)]);
        assert_eq!(
            resolve_anchor(&cs, Some(&h), None),
            (Some(5), AnchorMethod::OldSideLine)
        );
    }

    #[test]
    fn resolve_anchor_removed_not_covered_is_unresolved() {
        // e.g. the symbol was relocated in an earlier commit so the indexed
        // line no longer falls inside this diff's removed ranges: an honest
        // no-line beats a wrong line.
        let cs = changed(ChangeType::Removed, 50);
        let h = hunks(vec![], vec![(3, 8)]);
        assert_eq!(
            resolve_anchor(&cs, Some(&h), None),
            (None, AnchorMethod::Unresolved)
        );
    }

    #[test]
    fn resolve_anchor_removed_without_hunks_is_unresolved() {
        let cs = changed(ChangeType::Removed, 5);
        assert_eq!(
            resolve_anchor(&cs, None, None),
            (None, AnchorMethod::Unresolved)
        );
    }

    #[test]
    fn resolve_anchor_range_bounds_are_inclusive() {
        let cs = changed(ChangeType::Modified, 5);
        let h = hunks(vec![(5, 9)], vec![]);
        let cur = vec![current_symbol(5)];
        assert_eq!(
            resolve_anchor(&cs, Some(&h), Some(&cur)),
            (Some(5), AnchorMethod::NewSideHunk),
            "start bound must be inclusive"
        );
        let cur_end = vec![current_symbol(9)];
        assert_eq!(
            resolve_anchor(&cs, Some(&h), Some(&cur_end)),
            (Some(9), AnchorMethod::NewSideHunk),
            "end bound must be inclusive"
        );
    }

    #[test]
    fn resolve_anchor_prefers_lowest_current_line_on_duplicate_names() {
        let cs = changed(ChangeType::Modified, 1);
        let cur = vec![current_symbol(30), current_symbol(12)];
        assert_eq!(
            resolve_anchor(&cs, None, Some(&cur)),
            (Some(12), AnchorMethod::PostChangeFile)
        );
    }

    // -- Display strings ---------------------------------------------------------

    // -- anchored-line text (TASK-089, PRD-REV-REQ-013) -------------------------

    fn removed_hunks(lines: &[(usize, &str)]) -> FileDiffHunks {
        FileDiffHunks {
            removed_lines: lines.iter().map(|&(n, t)| (n, t.to_string())).collect(),
            ..hunks(vec![], vec![])
        }
    }

    #[test]
    fn anchored_line_text_new_side_reads_post_change_file() {
        let h = hunks(vec![(2, 2)], vec![]);
        let lines = vec!["one".to_string(), "pub fn f(x: i32) {".to_string()];
        assert_eq!(
            anchored_line_text(AnchorMethod::NewSideHunk, Some(2), Some(&h), Some(&lines)),
            Some("pub fn f(x: i32) {".to_string())
        );
    }

    #[test]
    fn anchored_line_text_post_change_reads_post_change_file() {
        let lines = vec!["one".to_string(), "two".to_string(), "fn g() {".to_string()];
        assert_eq!(
            anchored_line_text(AnchorMethod::PostChangeFile, Some(3), None, Some(&lines)),
            Some("fn g() {".to_string())
        );
    }

    #[test]
    fn anchored_line_text_old_side_reads_removed_lines_from_diff() {
        // Tier-2 anchors read the pre-change side: the removed `-` lines the
        // impact diff already carries, never the post-change file (whatever
        // now occupies that line is unrelated code).
        let h = removed_hunks(&[(1, "pub fn used() {}")]);
        let lines = vec!["pub fn caller() { used(); }".to_string()];
        assert_eq!(
            anchored_line_text(AnchorMethod::OldSideLine, Some(1), Some(&h), Some(&lines)),
            Some("pub fn used() {}".to_string())
        );
    }

    #[test]
    fn anchored_line_text_old_side_without_text_is_none() {
        assert_eq!(
            anchored_line_text(AnchorMethod::OldSideLine, Some(1), None, None),
            None,
            "no hunks, no old-side text"
        );
        let h = removed_hunks(&[(4, "x")]);
        assert_eq!(
            anchored_line_text(AnchorMethod::OldSideLine, Some(7), Some(&h), None),
            None,
            "line not among the removed lines"
        );
    }

    #[test]
    fn anchored_line_text_unresolved_is_none() {
        assert_eq!(
            anchored_line_text(AnchorMethod::Unresolved, None, None, None),
            None
        );
    }

    #[test]
    fn anchored_line_text_out_of_range_post_change_is_none() {
        // The file shrank below the anchored line: honest None over a panic
        // or a wrong line's text.
        let lines = vec!["one".to_string()];
        assert_eq!(
            anchored_line_text(AnchorMethod::PostChangeFile, Some(9), None, Some(&lines)),
            None
        );
    }

    // -- suppression storage (TASK-089, PRD-REV-REQ-014) ------------------------

    fn suppression_conn() -> (tempfile::TempDir, Connection) {
        let dir = tempfile::TempDir::new().unwrap();
        let conn = Connection::open(dir.path().join("index.db")).unwrap();
        crate::db::ensure_review_suppressions_table(&conn).unwrap();
        (dir, conn)
    }

    #[test]
    fn suppression_round_trip_lists_every_field() {
        let (_dir, conn) = suppression_conn();
        add_suppression(
            &conn,
            "abc123",
            "coverage-gap/no-test-in-blast-radius",
            "src/lib.rs",
            Some("confirmed false positive"),
        )
        .unwrap();

        let rows = list_suppressions(&conn, None).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].identity, "abc123");
        assert_eq!(rows[0].rule, "coverage-gap/no-test-in-blast-radius");
        assert_eq!(rows[0].file, "src/lib.rs");
        assert_eq!(rows[0].note.as_deref(), Some("confirmed false positive"));
        assert!(
            rows[0].created_at > 0,
            "created_at is epoch seconds, always positive"
        );
    }

    #[test]
    fn suppression_add_is_upsert_refreshing_rule_file_note() {
        // Re-adding an identity refreshes its display fields rather than
        // failing or duplicating the row.
        let (_dir, conn) = suppression_conn();
        add_suppression(&conn, "abc", "old-rule", "old.rs", Some("old note")).unwrap();
        add_suppression(&conn, "abc", "new-rule", "new.rs", None).unwrap();

        let rows = list_suppressions(&conn, None).unwrap();
        assert_eq!(rows.len(), 1, "upsert, not insert: {rows:?}");
        assert_eq!(rows[0].rule, "new-rule");
        assert_eq!(rows[0].file, "new.rs");
        assert_eq!(rows[0].note, None);
    }

    #[test]
    fn suppression_remove_by_identities_empties_and_counts() {
        let (_dir, conn) = suppression_conn();
        add_suppression(&conn, "a", "r", "f", None).unwrap();
        add_suppression(&conn, "b", "r", "f", None).unwrap();

        let removed = remove_suppressions(&conn, &["a".to_string()], None).unwrap();
        assert_eq!(removed, 1);
        let expected: HashSet<String> = ["b".to_string()].into_iter().collect();
        assert_eq!(suppressed_identities(&conn).unwrap(), expected);

        // Removing a missing identity counts zero, not an error.
        assert_eq!(remove_suppressions(&conn, &["a".into()], None).unwrap(), 0);
    }

    #[test]
    fn suppression_bulk_remove_by_rule_counts_only_matching() {
        let (_dir, conn) = suppression_conn();
        add_suppression(&conn, "a", "rule-one", "f", None).unwrap();
        add_suppression(&conn, "b", "rule-one", "f", None).unwrap();
        add_suppression(&conn, "c", "rule-two", "f", None).unwrap();

        assert_eq!(
            remove_suppressions(&conn, &[], Some("rule-one")).unwrap(),
            2,
            "only the rule's rows are removed"
        );
        let expected: HashSet<String> = ["c".to_string()].into_iter().collect();
        assert_eq!(suppressed_identities(&conn).unwrap(), expected);
    }

    #[test]
    fn suppression_list_filters_by_rule_and_orders_created_at_then_identity() {
        let (_dir, conn) = suppression_conn();
        add_suppression(&conn, "zzz", "wanted", "f", None).unwrap();
        add_suppression(&conn, "aaa", "wanted", "f", None).unwrap();
        add_suppression(&conn, "other", "unwanted", "f", None).unwrap();

        let rows = list_suppressions(&conn, Some("wanted")).unwrap();
        assert_eq!(
            rows.iter().map(|r| r.identity.as_str()).collect::<Vec<_>>(),
            vec!["aaa", "zzz"],
            "same created_at second falls back to ascending identity order"
        );

        let all = list_suppressions(&conn, None).unwrap();
        assert_eq!(all.len(), 3);
    }

    #[test]
    fn display_strings_are_stable() {
        assert_eq!(ReviewVerdict::Block.to_string(), "BLOCK");
        assert_eq!(ReviewVerdict::Review.to_string(), "REVIEW");
        assert_eq!(ReviewVerdict::Approve.to_string(), "APPROVE");
        assert_eq!(FindingSeverity::Blocking.to_string(), "blocking");
        assert_eq!(FindingSeverity::Warning.to_string(), "warning");
        assert_eq!(FindingSeverity::Note.to_string(), "note");
        assert_eq!(AnchorMethod::NewSideHunk.to_string(), "new-side-hunk");
        assert_eq!(AnchorMethod::OldSideLine.to_string(), "old-side-line");
        assert_eq!(AnchorMethod::PostChangeFile.to_string(), "post-change-file");
        assert_eq!(AnchorMethod::Unresolved.to_string(), "unresolved");
    }

    // -- fixture -----------------------------------------------------------------

    use rusqlite::Connection;
    use std::path::Path;
    use std::process::Command;
    use tempfile::TempDir;

    fn git_available() -> bool {
        Command::new("git")
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success())
    }

    /// Every stamped identity is 64 lowercase hex characters.
    fn assert_stable_identity(f: &Finding) {
        assert_hex_identity(&f.identity);
    }

    fn assert_hex_identity(identity: &str) {
        assert_eq!(
            identity.len(),
            64,
            "identity must be sha256 hex: {identity}"
        );
        assert!(
            identity.bytes().all(|b| b.is_ascii_hexdigit()),
            "identity must be hex: {identity}"
        );
    }

    /// Real git repo (diff scopes need commits) with an index reflecting the
    /// initial commit — the base state a review diff is taken against.
    fn make_review_repo(files: &[(&str, &str)]) -> (TempDir, Connection) {
        let dir = TempDir::new().unwrap();
        let root = dir.path();

        Command::new("git")
            .args(["init"])
            .current_dir(root)
            .output()
            .unwrap();
        Command::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(root)
            .output()
            .unwrap();
        Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(root)
            .output()
            .unwrap();

        for (path, content) in files {
            if let Some(parent) = Path::new(path).parent() {
                std::fs::create_dir_all(root.join(parent)).unwrap();
            }
            std::fs::write(root.join(path), content).unwrap();
        }

        Command::new("git")
            .args(["add", "."])
            .current_dir(root)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "initial"])
            .current_dir(root)
            .output()
            .unwrap();

        crate::pipeline::build_index(root, true).unwrap();
        let index_path = crate::db::local_index_path(root);
        let conn = crate::db::open_existing(&index_path).unwrap();
        (dir, conn)
    }

    // -- rule A: breaking change (PRD-REV-REQ-006) -------------------------------

    #[test]
    fn ac1_removing_called_function_blocks_anchored_to_old_side() {
        if !git_available() {
            return;
        }
        let (dir, conn) = make_review_repo(&[(
            "src/lib.rs",
            "pub fn used() {}\n\npub fn caller() { used(); }\n",
        )]);
        let root = dir.path();

        // Working tree deletes used(), keeps caller.
        std::fs::write(root.join("src/lib.rs"), "pub fn caller() { used(); }\n").unwrap();

        let result = run_review(
            &conn,
            &ChangeScope::Unstaged,
            root,
            &ReviewOptions::default(),
            None,
        )
        .unwrap();

        assert_eq!(result.verdict, ReviewVerdict::Block);
        assert_eq!(result.findings.len(), 1, "got: {:?}", result.findings);
        let f = &result.findings[0];
        assert_eq!(f.severity, FindingSeverity::Blocking);
        assert_eq!(f.kind, "breaking-change");
        assert_eq!(f.rule, "breaking-change/removed-symbol-with-callers");
        // Anchored to where used() WAS (old line 1), not to whatever now
        // occupies line 1 after the edit (AR-030).
        assert_eq!(f.anchor_method, AnchorMethod::OldSideLine);
        assert_eq!(f.line, Some(1));
        assert_eq!(f.related.len(), 1);
        assert_eq!(f.related[0].name, "caller");
        assert_eq!(
            f.message,
            "removed function `used` still has 1 indexed caller(s): caller"
        );
        assert_stable_identity(f);
    }

    #[test]
    fn rule_a_confidence_is_max_surviving_caller_edge_confidence() {
        // REQ-015: rule A's confidence is data-derived — the strongest
        // surviving caller edge — never a constant. A caller deleted in the
        // same diff is dead code and must not inflate it.
        if !git_available() {
            return;
        }
        let (dir, conn) = make_review_repo(&[(
            "src/lib.rs",
            "pub fn used() {}\n\npub fn keep_a() { used(); }\npub fn keep_b() { used(); }\n\npub fn dead() { used(); }\n",
        )]);
        let root = dir.path();

        for (caller, confidence) in [("keep_a", 0.6), ("keep_b", 0.9), ("dead", 1.0)] {
            conn.execute(
                "UPDATE \"references\" SET confidence = ?1 \
                 WHERE name = 'used' AND caller_id = \
                 (SELECT id FROM symbols WHERE name = ?2)",
                rusqlite::params![confidence, caller],
            )
            .unwrap();
        }

        // Working tree deletes used() AND dead() (dead code going with it).
        std::fs::write(
            root.join("src/lib.rs"),
            "pub fn keep_a() { used(); }\npub fn keep_b() { used(); }\n",
        )
        .unwrap();

        let result = run_review(
            &conn,
            &ChangeScope::Unstaged,
            root,
            &ReviewOptions {
                // The live BFS reads the reference rows this test edits; the
                // precomputed reach table would still hold the original
                // confidences (ac4b: both paths yield identical findings).
                reach_enabled: false,
                ..ReviewOptions::default()
            },
            None,
        )
        .unwrap();

        let f = result
            .findings
            .iter()
            .find(|f| f.kind == "breaking-change")
            .expect("breaking-change finding");
        assert_eq!(
            f.confidence, 0.9,
            "max SURVIVING edge confidence; dead's 1.0 must not count"
        );
    }

    #[test]
    fn rule_a_body_only_modified_with_callers_is_not_blocking() {
        // Blocking on any body edit would make BLOCK meaningless: only
        // removed or signature-changed symbols are candidates.
        if !git_available() {
            return;
        }
        let (dir, conn) = make_review_repo(&[(
            "src/lib.rs",
            "pub fn f() -> i32 { 1 }\n\npub fn caller() { f(); }\n",
        )]);
        let root = dir.path();

        std::fs::write(
            root.join("src/lib.rs"),
            "pub fn f() -> i32 { 2 }\n\npub fn caller() { f(); }\n",
        )
        .unwrap();

        let result = run_review(
            &conn,
            &ChangeScope::Unstaged,
            root,
            &ReviewOptions::default(),
            None,
        )
        .unwrap();

        assert!(
            result.findings.iter().all(|f| f.kind != "breaking-change"),
            "body-only modification must not block, got: {:?}",
            result.findings
        );
    }

    #[test]
    fn rule_a_removal_with_all_callers_removed_is_not_blocking() {
        // Deleting a helper and its only caller together is a refactor, not
        // a breaking change.
        if !git_available() {
            return;
        }
        let (dir, conn) = make_review_repo(&[(
            "src/lib.rs",
            "pub fn helper() {}\n\npub fn only_caller() { helper(); }\n",
        )]);
        let root = dir.path();

        std::fs::write(root.join("src/lib.rs"), "\n").unwrap();

        let result = run_review(
            &conn,
            &ChangeScope::Unstaged,
            root,
            &ReviewOptions::default(),
            None,
        )
        .unwrap();

        assert!(
            result.findings.iter().all(|f| f.kind != "breaking-change"),
            "dead-code removal must not block, got: {:?}",
            result.findings
        );
    }

    #[test]
    fn same_name_kind_in_another_file_is_not_dead_code() {
        // TASK-085 review debt: the removed-caller filter keyed on
        // (name, kind) only, so a LIVE caller sharing name+kind with an
        // unrelated removed symbol was dropped as dead code — and when it
        // was the only surviving caller, the BLOCK finding vanished.
        // Keying on (name, kind, file) keeps it.
        if !git_available() {
            return;
        }
        let (dir, conn) = make_review_repo(&[
            (
                "src/lib.rs",
                "pub fn run() {}\n\npub fn live_caller() { run(); }\n",
            ),
            (
                "src/other.rs",
                "pub fn run() {}\n\npub fn dead_caller() { run(); }\n",
            ),
        ]);
        let root = dir.path();

        // Delete other.rs entirely: `run`+`dead_caller` there are dead
        // code, but lib.rs's `run` and its LIVE caller `live_caller`
        // share the name — the filter must not drop them.
        std::fs::remove_file(root.join("src/other.rs")).unwrap();

        let result = run_review(
            &conn,
            &ChangeScope::Unstaged,
            root,
            &ReviewOptions::default(),
            None,
        )
        .unwrap();

        let removed_run = result.findings.iter().any(|f| {
            f.kind == "breaking-change"
                && f.rule.contains("removed-symbol-with-callers")
                && f.message.contains("run")
        });
        assert!(
            removed_run,
            "the live caller of the removed `run` must still block, got: {:?}",
            result.findings
        );
    }

    // -- rule B: coverage gap (PRD-REV-REQ-007) ----------------------------------

    #[test]
    fn ac2_changed_symbol_without_test_coverage_warns() {
        if !git_available() {
            return;
        }
        let (dir, conn) = make_review_repo(&[(
            "src/lib.rs",
            "pub fn f() -> i32 { 1 }\npub fn g() -> i32 { f() }\n",
        )]);
        let root = dir.path();

        // Body edit: not breaking, but nothing anywhere exercises f.
        std::fs::write(
            root.join("src/lib.rs"),
            "pub fn f() -> i32 { 2 }\npub fn g() -> i32 { f() }\n",
        )
        .unwrap();

        let result = run_review(
            &conn,
            &ChangeScope::Unstaged,
            root,
            &ReviewOptions::default(),
            None,
        )
        .unwrap();

        assert_eq!(
            result.verdict,
            ReviewVerdict::Review,
            "a warning must yield REVIEW, got {:?}",
            result.findings
        );
        assert_eq!(result.findings.len(), 1, "got: {:?}", result.findings);
        let f = &result.findings[0];
        assert_eq!(f.severity, FindingSeverity::Warning);
        assert_eq!(f.kind, "coverage-gap");
        assert_eq!(f.rule, "coverage-gap/no-test-in-blast-radius");
        assert_eq!(f.confidence, COVERAGE_GAP_CONFIDENCE);
        assert_eq!(
            f.message,
            "function `f` changed but no test file appears in its blast radius (1 affected symbol(s), none in tests)"
        );
        // Related context is the canonical (tests-excluded) blast radius.
        assert_eq!(f.related.len(), 1);
        assert_eq!(f.related[0].name, "g");
    }

    #[test]
    fn suppressed_identity_retires_its_finding_and_flips_the_verdict() {
        // REQ-014's contract: suppress the identity a first review stamped,
        // re-run the same diff, and the finding is gone — the verdict
        // derives from what is reported, so REVIEW becomes APPROVE.
        if !git_available() {
            return;
        }
        let (dir, conn) = make_review_repo(&[(
            "src/lib.rs",
            "pub fn f() -> i32 { 1 }\npub fn g() -> i32 { f() }\n",
        )]);
        let root = dir.path();
        std::fs::write(
            root.join("src/lib.rs"),
            "pub fn f() -> i32 { 2 }\npub fn g() -> i32 { f() }\n",
        )
        .unwrap();

        let before = run_review(
            &conn,
            &ChangeScope::Unstaged,
            root,
            &ReviewOptions::default(),
            None,
        )
        .unwrap();
        assert_eq!(before.verdict, ReviewVerdict::Review);
        assert_eq!(before.findings.len(), 1);
        let gap = &before.findings[0];
        add_suppression(&conn, &gap.identity, &gap.rule, &gap.file, None).unwrap();

        let after = run_review(
            &conn,
            &ChangeScope::Unstaged,
            root,
            &ReviewOptions::default(),
            None,
        )
        .unwrap();
        assert!(
            after.findings.is_empty(),
            "suppressed finding must not be kept, got: {:?}",
            after.findings
        );
        assert_eq!(after.verdict, ReviewVerdict::Approve);
        assert_eq!(
            after.drops,
            DropCounts {
                identity_suppressed: 1,
                ..DropCounts::default()
            },
            "the drop is attributed to suppression alone — never also over_cap"
        );
        assert_eq!(before.drops, DropCounts::default());
    }

    // -- identity acceptance criteria (TASK-089, PRD-REV-REQ-013/AR-031) -------
    //
    // Multi-line functions so the anchor is the signature line and a body
    // edit never touches it.

    const AC_F_BASE: &str = "pub fn f() -> i32 {\n    1\n}\n\npub fn g() -> i32 {\n    f()\n}\n";

    fn git_cmd(root: &Path, args: &[&str]) {
        let out = Command::new("git")
            .args(args)
            .current_dir(root)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {:?} failed: {args:?}", args);
    }

    fn commit_and_reindex(root: &Path) {
        git_cmd(root, &["add", "."]);
        git_cmd(root, &["commit", "-m", "update"]);
        crate::pipeline::build_index(root, true).unwrap();
    }

    fn coverage_gap_of(result: &ReviewResult) -> &Finding {
        result
            .findings
            .iter()
            .find(|f| f.kind == "coverage-gap")
            .unwrap_or_else(|| panic!("expected a coverage-gap finding: {:?}", result.findings))
    }

    #[test]
    fn ac1_reformatting_the_flagged_line_preserves_identity_and_suppression() {
        if !git_available() {
            return;
        }
        let (dir, conn) = make_review_repo(&[("src/lib.rs", AC_F_BASE)]);
        let root = dir.path();
        std::fs::write(
            root.join("src/lib.rs"),
            "pub fn f() -> i32 {\n    2\n}\n\npub fn g() -> i32 {\n    f()\n}\n",
        )
        .unwrap();
        let first = run_review(
            &conn,
            &ChangeScope::Unstaged,
            root,
            &ReviewOptions::default(),
            None,
        )
        .unwrap();
        let identity = coverage_gap_of(&first).identity.clone();
        assert_stable_identity(coverage_gap_of(&first));
        add_suppression(
            &conn,
            &identity,
            "coverage-gap/no-test-in-blast-radius",
            "src/lib.rs",
            None,
        )
        .unwrap();

        // Commit the edit, re-index, then re-indent f's block AND change its
        // body again: the anchored line's tokens are identical modulo
        // whitespace, so the identity — and with it the suppression — must
        // survive (PRD-REV-REQ-013).
        commit_and_reindex(root);
        std::fs::write(
            root.join("src/lib.rs"),
            "    pub fn f() -> i32 {\n        3\n    }\n\npub fn g() -> i32 {\n    f()\n}\n",
        )
        .unwrap();

        let second = run_review(
            &conn,
            &ChangeScope::Unstaged,
            root,
            &ReviewOptions::default(),
            None,
        )
        .unwrap();
        assert!(
            second.findings.iter().all(|f| f.kind != "coverage-gap"),
            "reformatted finding stays suppressed: {:?}",
            second.findings
        );
        assert_eq!(second.drops.identity_suppressed, 1);

        // Un-suppress: the reformatted finding returns with the SAME
        // identity — proof the suppression matched by identity, not absence.
        remove_suppressions(&conn, std::slice::from_ref(&identity), None).unwrap();
        let third = run_review(
            &conn,
            &ChangeScope::Unstaged,
            root,
            &ReviewOptions::default(),
            None,
        )
        .unwrap();
        assert_eq!(coverage_gap_of(&third).identity, identity);
    }

    #[test]
    fn ac2_inserting_lines_above_preserves_identity() {
        if !git_available() {
            return;
        }
        let (dir, conn) = make_review_repo(&[("src/lib.rs", AC_F_BASE)]);
        let root = dir.path();
        std::fs::write(
            root.join("src/lib.rs"),
            "pub fn f() -> i32 {\n    2\n}\n\npub fn g() -> i32 {\n    f()\n}\n",
        )
        .unwrap();
        let first = run_review(
            &conn,
            &ChangeScope::Unstaged,
            root,
            &ReviewOptions::default(),
            None,
        )
        .unwrap();
        let identity = coverage_gap_of(&first).identity.clone();

        // Ten unrelated lines land above f: the anchored line moves down,
        // the identity must not move with it (the line number is
        // structurally absent from the signature).
        commit_and_reindex(root);
        let pads = "// pad line\n".repeat(10);
        std::fs::write(
            root.join("src/lib.rs"),
            format!("{pads}pub fn f() -> i32 {{\n    3\n}}\n\npub fn g() -> i32 {{\n    f()\n}}\n"),
        )
        .unwrap();

        let second = run_review(
            &conn,
            &ChangeScope::Unstaged,
            root,
            &ReviewOptions::default(),
            None,
        )
        .unwrap();
        let gap = coverage_gap_of(&second);
        assert_eq!(gap.line, Some(11), "the anchor DID move: {:?}", gap.line);
        assert_eq!(gap.identity, identity, "identity must not track the line");
    }

    #[test]
    fn ac3_changing_the_flagged_code_changes_identity_and_unmasks() {
        if !git_available() {
            return;
        }
        let (dir, conn) = make_review_repo(&[("src/lib.rs", AC_F_BASE)]);
        let root = dir.path();
        std::fs::write(
            root.join("src/lib.rs"),
            "pub fn f() -> i32 {\n    2\n}\n\npub fn g() -> i32 {\n    f()\n}\n",
        )
        .unwrap();
        let first = run_review(
            &conn,
            &ChangeScope::Unstaged,
            root,
            &ReviewOptions::default(),
            None,
        )
        .unwrap();
        let identity = coverage_gap_of(&first).identity.clone();
        add_suppression(
            &conn,
            &identity,
            "coverage-gap/no-test-in-blast-radius",
            "src/lib.rs",
            None,
        )
        .unwrap();

        // Change the anchored signature line itself: a genuinely different
        // finding at the same site must NOT inherit the suppression (AR-031).
        commit_and_reindex(root);
        std::fs::write(
            root.join("src/lib.rs"),
            "pub fn f(x: i32) -> i32 {\n    x + 3\n}\n\npub fn g() -> i32 {\n    f(1)\n}\n",
        )
        .unwrap();

        let second = run_review(
            &conn,
            &ChangeScope::Unstaged,
            root,
            &ReviewOptions::default(),
            None,
        )
        .unwrap();
        let gap = coverage_gap_of(&second);
        assert_ne!(gap.identity, identity, "token change must re-key");
        assert_stable_identity(gap);
        assert_eq!(
            second.drops.identity_suppressed, 0,
            "the stale suppression masks nothing: {:?}",
            second.drops
        );
    }

    #[test]
    fn old_side_deletion_identity_comes_from_the_removed_line_text() {
        if !git_available() {
            return;
        }
        // Two bases differing ONLY in f's signature line text; deleting f
        // from each must yield different identities — the pre-change text
        // feeds the hash (the line no longer exists post-change).
        let (dir_a, conn_a) = make_review_repo(&[(
            "src/lib.rs",
            "pub fn f() -> i32 {\n    1\n}\n\npub fn g() -> i32 {\n    f()\n}\n",
        )]);
        let (dir_b, conn_b) = make_review_repo(&[(
            "src/lib.rs",
            "pub fn f(x: i32) -> i32 {\n    x\n}\n\npub fn g() -> i32 {\n    f(1)\n}\n",
        )]);

        let deleted = "pub fn g() -> i32 {\n    f()\n}\n";
        std::fs::write(dir_a.path().join("src/lib.rs"), deleted).unwrap();
        std::fs::write(dir_b.path().join("src/lib.rs"), deleted).unwrap();

        let find = |dir: &TempDir, conn: &Connection| {
            let result = run_review(
                conn,
                &ChangeScope::Unstaged,
                dir.path(),
                &ReviewOptions::default(),
                None,
            )
            .unwrap();
            let f = result
                .findings
                .iter()
                .find(|f| f.kind == "breaking-change")
                .unwrap_or_else(|| panic!("expected a breaking change: {:?}", result.findings));
            assert_eq!(f.anchor_method, AnchorMethod::OldSideLine);
            (f.identity.clone(), f.line)
        };
        let (id_a, line_a) = find(&dir_a, &conn_a);
        let (id_b, line_b) = find(&dir_b, &conn_b);
        assert_hex_identity(&id_a);
        assert_hex_identity(&id_b);
        assert_eq!(line_a, line_b, "same old-side line in both repos");
        assert_ne!(
            id_a, id_b,
            "identities differ only through the removed-line text"
        );
    }

    #[test]
    fn ac2_test_in_blast_radius_silences_coverage_gap() {
        if !git_available() {
            return;
        }
        let (dir, conn) = make_review_repo(&[
            (
                "src/lib.rs",
                "pub fn f() -> i32 { 1 }\npub fn g() -> i32 { f() }\n",
            ),
            ("tests/x.rs", "fn t() { f(); }\n"),
        ]);
        let root = dir.path();

        std::fs::write(
            root.join("src/lib.rs"),
            "pub fn f() -> i32 { 2 }\npub fn g() -> i32 { f() }\n",
        )
        .unwrap();

        let result = run_review(
            &conn,
            &ChangeScope::Unstaged,
            root,
            &ReviewOptions::default(),
            None,
        )
        .unwrap();

        assert!(
            result.findings.iter().all(|f| f.kind != "coverage-gap"),
            "a test in the radius covers the change, got: {:?}",
            result.findings
        );
        assert_eq!(result.verdict, ReviewVerdict::Approve);
    }

    #[test]
    fn rule_b_skips_removed_symbols() {
        // Rule A governs removals; a coverage warning about deleted code is
        // noise.
        if !git_available() {
            return;
        }
        let (dir, conn) = make_review_repo(&[(
            "src/lib.rs",
            "pub fn f() -> i32 { 1 }\npub fn g() -> i32 { f() }\n",
        )]);
        let root = dir.path();

        std::fs::write(root.join("src/lib.rs"), "pub fn g() -> i32 { 0 }\n").unwrap();

        let result = run_review(
            &conn,
            &ChangeScope::Unstaged,
            root,
            &ReviewOptions::default(),
            None,
        )
        .unwrap();

        assert!(
            result.findings.iter().all(|f| f.kind != "coverage-gap"),
            "removed symbols must not raise coverage gaps, got: {:?}",
            result.findings
        );
        assert_eq!(result.verdict, ReviewVerdict::Block);
    }

    #[test]
    fn rule_b_skips_symbols_in_test_files() {
        if !git_available() {
            return;
        }
        let (dir, conn) = make_review_repo(&[
            (
                "src/lib.rs",
                "pub fn f() -> i32 { 1 }\npub fn g() -> i32 { f() }\n",
            ),
            ("tests/x.rs", "fn t() { f(); }\n"),
        ]);
        let root = dir.path();

        std::fs::write(root.join("tests/x.rs"), "fn t() { f(); let _ = 1; }\n").unwrap();

        let result = run_review(
            &conn,
            &ChangeScope::Unstaged,
            root,
            &ReviewOptions::default(),
            None,
        )
        .unwrap();

        assert!(
            result.findings.iter().all(|f| f.kind != "coverage-gap"),
            "changes inside test files are themselves coverage, got: {:?}",
            result.findings
        );
    }

    #[test]
    fn rule_b_empty_radius_message() {
        if !git_available() {
            return;
        }
        let (dir, conn) = make_review_repo(&[("src/lib.rs", "pub fn solo() -> i32 { 1 }\n")]);
        let root = dir.path();

        // Nobody calls solo anywhere: the radius is empty.
        std::fs::write(root.join("src/lib.rs"), "pub fn solo() -> i32 { 2 }\n").unwrap();

        let result = run_review(
            &conn,
            &ChangeScope::Unstaged,
            root,
            &ReviewOptions::default(),
            None,
        )
        .unwrap();

        let gaps: Vec<_> = result
            .findings
            .iter()
            .filter(|f| f.kind == "coverage-gap")
            .collect();
        assert_eq!(gaps.len(), 1);
        assert_eq!(
            gaps[0].message,
            "function `solo` changed but no test file appears in its blast radius (no affected symbols indexed)"
        );
        assert!(gaps[0].related.is_empty());
    }

    // -- never-disagree (AC4) ----------------------------------------------------

    fn symbol_refs_of(analysis: &crate::types::BlastAnalysis) -> Vec<SymbolRef> {
        analysis
            .tiers
            .iter()
            .flat_map(|t| t.symbols.iter())
            .map(SymbolRef::from)
            .collect()
    }

    #[test]
    fn ac4a_rule_a_related_equals_standalone_blast_output() {
        if !git_available() {
            return;
        }
        let (dir, conn) = make_review_repo(&[(
            "src/lib.rs",
            "pub fn used() {}\n\npub fn caller() { used(); }\n",
        )]);
        let root = dir.path();

        std::fs::write(root.join("src/lib.rs"), "pub fn caller() { used(); }\n").unwrap();

        let result = run_review(
            &conn,
            &ChangeScope::Unstaged,
            root,
            &ReviewOptions::default(),
            None,
        )
        .unwrap();

        // Standalone blast with wonk blast defaults, run beside the review.
        let standalone = blast::analyze_blast(
            &conn,
            "used",
            &BlastOptions {
                depth: blast::DEFAULT_DEPTH,
                direction: BlastDirection::Upstream,
                include_tests: false,
                min_confidence: None,
                use_reach: true,
            },
        )
        .unwrap();

        let finding = result
            .findings
            .iter()
            .find(|f| f.kind == "breaking-change")
            .expect("breaking-change finding");
        let will_break: Vec<SymbolRef> = standalone
            .tiers
            .iter()
            .find(|t| t.severity == BlastSeverity::WillBreak)
            .map(|t| t.symbols.iter().map(SymbolRef::from).collect())
            .unwrap_or_default();
        assert_eq!(finding.related, will_break);
    }

    #[test]
    fn ac4a_rule_b_related_equals_default_blast_full_tiers() {
        if !git_available() {
            return;
        }
        let (dir, conn) = make_review_repo(&[(
            "src/lib.rs",
            "pub fn f() -> i32 { 1 }\npub fn g() -> i32 { f() }\npub fn h() { g(); }\n",
        )]);
        let root = dir.path();

        std::fs::write(
            root.join("src/lib.rs"),
            "pub fn f() -> i32 { 2 }\npub fn g() -> i32 { f() }\npub fn h() { g(); }\n",
        )
        .unwrap();

        let result = run_review(
            &conn,
            &ChangeScope::Unstaged,
            root,
            &ReviewOptions::default(),
            None,
        )
        .unwrap();

        let standalone = blast::analyze_blast(&conn, "f", &BlastOptions::default()).unwrap();

        let finding = result
            .findings
            .iter()
            .find(|f| f.kind == "coverage-gap")
            .expect("coverage-gap finding");
        // Related is the canonical tests-excluded radius across all tiers.
        assert_eq!(finding.related, symbol_refs_of(&standalone));
    }

    #[test]
    fn ac4b_reach_kill_switch_yields_identical_findings() {
        if !git_available() {
            return;
        }
        let (dir, conn) = make_review_repo(&[
            (
                "src/lib.rs",
                "pub fn f(x: i32) -> i32 { x }\npub fn g() -> i32 { f(1) }\n",
            ),
            ("tests/x.rs", "fn t() { let _ = g(); }\n"),
        ]);
        let root = dir.path();

        // Signature change (rule A candidate) plus radius touching a test
        // file, so both rule families run with survivors.
        std::fs::write(
            root.join("src/lib.rs"),
            "pub fn f(x: i64) -> i64 { x }\npub fn g() -> i64 { f(1) }\n",
        )
        .unwrap();

        let with_reach = run_review(
            &conn,
            &ChangeScope::Unstaged,
            root,
            &ReviewOptions::default(),
            None,
        )
        .unwrap();
        let without_reach = run_review(
            &conn,
            &ChangeScope::Unstaged,
            root,
            &ReviewOptions {
                reach_enabled: false,
                ..ReviewOptions::default()
            },
            None,
        )
        .unwrap();

        assert_eq!(with_reach.findings, without_reach.findings);
        assert_eq!(with_reach.verdict, without_reach.verdict);
        assert_eq!(with_reach.verdict, ReviewVerdict::Block);
    }

    // -- extra fixtures ----------------------------------------------------------

    #[test]
    fn stale_index_relocation_yields_unresolved_anchor_not_wrong_line() {
        // Index built at commit A; commit B relocates the fn without
        // re-indexing; then the fn is deleted. The stale indexed line no
        // longer falls inside this diff's removed ranges, so the finding is
        // emitted without a line rather than pointing at the wrong one.
        if !git_available() {
            return;
        }
        let (dir, conn) = make_review_repo(&[(
            "src/lib.rs",
            "pub fn relocated() {}\n\npub fn caller() { relocated(); }\n",
        )]);
        let root = dir.path();

        // Commit B: insert 30 lines above relocated() (index stays at A).
        let mut content = String::new();
        for i in 0..30 {
            content.push_str(&format!("const PAD_{i}: i32 = {i};\n"));
        }
        content.push_str("\npub fn relocated() {}\n\npub fn caller() { relocated(); }\n");
        std::fs::write(root.join("src/lib.rs"), &content).unwrap();
        Command::new("git")
            .args(["add", "."])
            .current_dir(root)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "relocate"])
            .current_dir(root)
            .output()
            .unwrap();

        // Delete relocated() from the working tree.
        let without = content
            .replace("\npub fn relocated() {}\n", "\n")
            .replace("pub fn caller() { relocated(); }", "pub fn caller() {}");
        std::fs::write(root.join("src/lib.rs"), without).unwrap();

        let result = run_review(
            &conn,
            &ChangeScope::Unstaged,
            root,
            &ReviewOptions::default(),
            None,
        )
        .unwrap();

        let finding = result
            .findings
            .iter()
            .find(|f| f.kind == "breaking-change")
            .expect("breaking-change finding must still be emitted");
        assert_eq!(finding.anchor_method, AnchorMethod::Unresolved);
        assert_eq!(finding.line, None);
        // Identity is stamped even when the anchor did not resolve — the
        // suppression key space covers unanchored findings too.
        assert_stable_identity(finding);
    }

    #[test]
    fn rules_independently_disableable() {
        if !git_available() {
            return;
        }
        let (dir, conn) = make_review_repo(&[(
            "src/lib.rs",
            "pub fn used() {}\n\npub fn caller() { used(); }\n",
        )]);
        let root = dir.path();
        std::fs::write(root.join("src/lib.rs"), "pub fn caller() { used(); }\n").unwrap();

        let result = run_review(
            &conn,
            &ChangeScope::Unstaged,
            root,
            &ReviewOptions {
                breaking_change: false,
                ..ReviewOptions::default()
            },
            None,
        )
        .unwrap();
        assert!(
            result.findings.is_empty(),
            "rule A disabled must drop its findings, got: {:?}",
            result.findings
        );
        assert_eq!(result.verdict, ReviewVerdict::Approve);
    }

    #[test]
    fn rule_b_independently_disableable() {
        // TASK-085 review debt (AR-022): only family A's disable path was
        // verified — a miswired coverage_gap gate would pass the suite
        // silently. A would-be coverage-gap diff under
        // `coverage_gap: false` must yield no coverage findings.
        if !git_available() {
            return;
        }
        let (dir, conn) = make_review_repo(&[(
            "src/lib.rs",
            "pub fn f() -> i32 { 1 }\npub fn g() -> i32 { f() }\n",
        )]);
        let root = dir.path();
        // Body edit with no test coverage anywhere: a rule-B candidate.
        std::fs::write(
            root.join("src/lib.rs"),
            "pub fn f() -> i32 { 2 }\npub fn g() -> i32 { f() }\n",
        )
        .unwrap();

        let result = run_review(
            &conn,
            &ChangeScope::Unstaged,
            root,
            &ReviewOptions {
                coverage_gap: false,
                ..ReviewOptions::default()
            },
            None,
        )
        .unwrap();
        assert!(
            result.findings.iter().all(|f| f.kind != "coverage-gap"),
            "rule B disabled must drop its findings, got: {:?}",
            result.findings
        );
        assert_eq!(result.verdict, ReviewVerdict::Approve);

        // The same diff with the rule on must produce the finding — the
        // fixture is a real candidate, not an empty one.
        let enabled = run_review(
            &conn,
            &ChangeScope::Unstaged,
            root,
            &ReviewOptions::default(),
            None,
        )
        .unwrap();
        assert!(
            enabled.findings.iter().any(|f| f.kind == "coverage-gap"),
            "fixture sanity: the enabled run must find the coverage gap"
        );
    }

    #[test]
    fn staged_scope_reviews_staged_edits() {
        if !git_available() {
            return;
        }
        let (dir, conn) = make_review_repo(&[(
            "src/lib.rs",
            "pub fn used() {}\n\npub fn caller() { used(); }\n",
        )]);
        let root = dir.path();
        std::fs::write(root.join("src/lib.rs"), "pub fn caller() { used(); }\n").unwrap();
        Command::new("git")
            .args(["add", "src/lib.rs"])
            .current_dir(root)
            .output()
            .unwrap();

        let result = run_review(
            &conn,
            &ChangeScope::Staged,
            root,
            &ReviewOptions::default(),
            None,
        )
        .unwrap();
        assert_eq!(result.verdict, ReviewVerdict::Block);
        assert_eq!(result.findings.len(), 1);
    }

    #[test]
    fn compare_scope_reviews_against_base_ref() {
        if !git_available() {
            return;
        }
        let (dir, conn) = make_review_repo(&[(
            "src/lib.rs",
            "pub fn used() {}\n\npub fn caller() { used(); }\n",
        )]);
        let root = dir.path();
        std::fs::write(root.join("src/lib.rs"), "pub fn caller() { used(); }\n").unwrap();
        Command::new("git")
            .args(["add", "."])
            .current_dir(root)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "remove used"])
            .current_dir(root)
            .output()
            .unwrap();

        let result = run_review(
            &conn,
            &ChangeScope::Compare("HEAD~1".into()),
            root,
            &ReviewOptions::default(),
            None,
        )
        .unwrap();
        assert_eq!(result.verdict, ReviewVerdict::Block);
        assert_eq!(result.findings.len(), 1);
    }

    // -- rule C: cross-repo impact (TASK-086, PRD-REV-REQ-010) ------------------

    const CROSS_REPO_ROUTES: &str = "const app = express();\nfunction registerUserRoutes() {\n  app.get('/v1/users', getUser);\n}\n";
    const CROSS_REPO_ROUTES_EDITED: &str = "const app = express();\nfunction registerUserRoutes() {\n  app.get('/v1/users', getUserV2);\n}\n";
    const SIBLING_CLIENT: &str =
        "async function loadUsers() {\n  await fetch('https://api.io/v1/users');\n}\n";

    /// A registered sibling repo (blast's `registered_contract_repo`
    /// pattern): indexed and published to `repos_dir`, no git history —
    /// it is never the repo under review.
    fn registered_sibling(
        repos_dir: &Path,
        name: &str,
        workspace: &str,
        files: &[(&str, &str)],
    ) -> TempDir {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join(name);
        std::fs::create_dir_all(root.join(".git")).unwrap();
        for (path, content) in files {
            if let Some(parent) = Path::new(path).parent() {
                std::fs::create_dir_all(root.join(parent)).unwrap();
            }
            std::fs::write(root.join(path), content).unwrap();
        }
        std::fs::create_dir_all(root.join(".wonk")).unwrap();
        std::fs::write(
            root.join(".wonk/config.toml"),
            format!("[contracts]\nworkspace = \"{workspace}\"\n"),
        )
        .unwrap();
        crate::pipeline::build_index(&root, true).unwrap();
        let dest = repos_dir.join(crate::db::repo_hash(&root));
        std::fs::create_dir_all(&dest).unwrap();
        std::fs::copy(root.join(".wonk/index.db"), dest.join("index.db")).unwrap();
        std::fs::copy(root.join(".wonk/meta.json"), dest.join("meta.json")).unwrap();
        dir
    }

    /// The reviewed provider repo: real git history (diff scopes need
    /// commits, `make_review_repo`'s half) PLUS central registration and a
    /// workspace declaration (rule C's half). Returns the TempDir keeping
    /// everything alive, the repo root, and the LOCAL index connection
    /// review runs against.
    fn make_cross_repo_review_repo(
        repos_dir: &Path,
        name: &str,
        workspace: &str,
        files: &[(&str, &str)],
    ) -> (TempDir, std::path::PathBuf, Connection) {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join(name);
        std::fs::create_dir_all(&root).unwrap();
        Command::new("git")
            .args(["init"])
            .current_dir(&root)
            .output()
            .unwrap();
        Command::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(&root)
            .output()
            .unwrap();
        Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(&root)
            .output()
            .unwrap();
        for (path, content) in files {
            if let Some(parent) = Path::new(path).parent() {
                std::fs::create_dir_all(root.join(parent)).unwrap();
            }
            std::fs::write(root.join(path), content).unwrap();
        }
        std::fs::create_dir_all(root.join(".wonk")).unwrap();
        std::fs::write(
            root.join(".wonk/config.toml"),
            format!("[contracts]\nworkspace = \"{workspace}\"\n"),
        )
        .unwrap();
        Command::new("git")
            .args(["add", "."])
            .current_dir(&root)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "initial"])
            .current_dir(&root)
            .output()
            .unwrap();

        crate::pipeline::build_index(&root, true).unwrap();
        let dest = repos_dir.join(crate::db::repo_hash(&root));
        std::fs::create_dir_all(&dest).unwrap();
        std::fs::copy(root.join(".wonk/index.db"), dest.join("index.db")).unwrap();
        std::fs::copy(root.join(".wonk/meta.json"), dest.join("meta.json")).unwrap();
        let conn = crate::db::open_existing(&root.join(".wonk/index.db")).unwrap();
        (dir, root, conn)
    }

    #[test]
    fn ac1_changed_provider_with_sibling_consumer_warns_cross_repo() {
        if !git_available() {
            return;
        }
        // Rebuild the setup inline: make_cross_repo_review_setup cannot
        // return the provider repo (it owns three TempDirs), and the test
        // must edit the provider's working tree.
        let repos_dir = TempDir::new().unwrap();
        let (_own_dir, root, conn) = make_cross_repo_review_repo(
            repos_dir.path(),
            "users-svc",
            "payments",
            &[("src/routes.js", CROSS_REPO_ROUTES)],
        );
        let _sib = registered_sibling(
            repos_dir.path(),
            "own-api",
            "payments",
            &[("src/client.js", SIBLING_CLIENT)],
        );
        let inputs = CrossRepoInputs {
            declared: vec!["payments".to_string()],
            repos_dir: repos_dir.path().to_path_buf(),
        };

        // Body edit: the route's handler argument changes, the signature
        // does not. Siblings depend on behavior, so this must warn.
        std::fs::write(root.join("src/routes.js"), CROSS_REPO_ROUTES_EDITED).unwrap();

        let result = run_review(
            &conn,
            &ChangeScope::Unstaged,
            &root,
            &ReviewOptions::default(),
            Some(&inputs),
        )
        .unwrap();

        let cross: Vec<_> = result
            .findings
            .iter()
            .filter(|f| f.kind == "cross-repo")
            .collect();
        assert_eq!(cross.len(), 1, "got: {:?}", result.findings);
        let f = cross[0];
        assert_eq!(f.severity, FindingSeverity::Warning);
        assert_eq!(
            f.rule,
            "cross-repo/changed-provider-with-external-consumers"
        );
        assert_eq!(f.confidence, CROSS_REPO_CONFIDENCE);
        assert_eq!(
            f.message,
            "function `registerUserRoutes` changed but provides contract(s) http::GET::/v1/users consumed by 1 other repo(s): own-api"
        );
        assert!(f.line.is_some(), "rule C reuses the loop's anchor");
        assert_eq!(f.related.len(), 1);
        assert_eq!(f.related[0].file, "own-api:src/client.js");
        assert_eq!(f.related[0].name, "loadUsers");
        assert_eq!(result.verdict, ReviewVerdict::Review);
    }

    #[test]
    fn rule_c_disabled_by_option() {
        if !git_available() {
            return;
        }
        let repos_dir = TempDir::new().unwrap();
        let (_own_dir, root, conn) = make_cross_repo_review_repo(
            repos_dir.path(),
            "users-svc",
            "payments",
            &[("src/routes.js", CROSS_REPO_ROUTES)],
        );
        let _sib = registered_sibling(
            repos_dir.path(),
            "own-api",
            "payments",
            &[("src/client.js", SIBLING_CLIENT)],
        );
        let inputs = CrossRepoInputs {
            declared: vec!["payments".to_string()],
            repos_dir: repos_dir.path().to_path_buf(),
        };
        std::fs::write(root.join("src/routes.js"), CROSS_REPO_ROUTES_EDITED).unwrap();

        let result = run_review(
            &conn,
            &ChangeScope::Unstaged,
            &root,
            &ReviewOptions {
                cross_repo: false,
                ..ReviewOptions::default()
            },
            Some(&inputs),
        )
        .unwrap();
        assert!(
            result.findings.iter().all(|f| f.kind != "cross-repo"),
            "kill switch must drop rule C, got: {:?}",
            result.findings
        );
    }

    #[test]
    fn rule_c_sibling_in_other_workspace_no_finding() {
        if !git_available() {
            return;
        }
        let repos_dir = TempDir::new().unwrap();
        let (_own_dir, root, conn) = make_cross_repo_review_repo(
            repos_dir.path(),
            "users-svc",
            "payments",
            &[("src/routes.js", CROSS_REPO_ROUTES)],
        );
        let _sib = registered_sibling(
            repos_dir.path(),
            "own-api",
            "billing",
            &[("src/client.js", SIBLING_CLIENT)],
        );
        let inputs = CrossRepoInputs {
            declared: vec!["payments".to_string()],
            repos_dir: repos_dir.path().to_path_buf(),
        };
        std::fs::write(root.join("src/routes.js"), CROSS_REPO_ROUTES_EDITED).unwrap();

        let result = run_review(
            &conn,
            &ChangeScope::Unstaged,
            &root,
            &ReviewOptions::default(),
            Some(&inputs),
        )
        .unwrap();
        assert!(
            result.findings.iter().all(|f| f.kind != "cross-repo"),
            "workspaces must not intersect, got: {:?}",
            result.findings
        );
    }

    #[test]
    fn rule_c_removed_provider_with_sibling_consumer_warns_cross_repo() {
        // REQ-010's trigger is "a changed symbol providing a contract
        // consumed by another indexed repo" — removals included. Rule A
        // cannot see these consumers (its callers are the in-repo WillBreak
        // tier), so rule C is the only surface that can name the sibling;
        // without it the highest-impact cross-repo change would be silent.
        if !git_available() {
            return;
        }
        let repos_dir = TempDir::new().unwrap();
        let (_own_dir, root, conn) = make_cross_repo_review_repo(
            repos_dir.path(),
            "users-svc",
            "payments",
            &[("src/routes.js", CROSS_REPO_ROUTES)],
        );
        let _sib = registered_sibling(
            repos_dir.path(),
            "own-api",
            "payments",
            &[("src/client.js", SIBLING_CLIENT)],
        );
        let inputs = CrossRepoInputs {
            declared: vec!["payments".to_string()],
            repos_dir: repos_dir.path().to_path_buf(),
        };
        // Remove registerUserRoutes entirely (no in-repo callers: rule A
        // stays silent too, so the finding below is rule C's alone). The
        // base-state index still holds its provider contract rows.
        std::fs::write(root.join("src/routes.js"), "const app = express();\n").unwrap();

        let result = run_review(
            &conn,
            &ChangeScope::Unstaged,
            &root,
            &ReviewOptions::default(),
            Some(&inputs),
        )
        .unwrap();

        let cross: Vec<_> = result
            .findings
            .iter()
            .filter(|f| f.kind == "cross-repo")
            .collect();
        assert_eq!(cross.len(), 1, "got: {:?}", result.findings);
        let f = cross[0];
        assert_eq!(f.severity, FindingSeverity::Warning);
        assert_eq!(
            f.rule,
            "cross-repo/changed-provider-with-external-consumers"
        );
        assert_eq!(
            f.message,
            "removed function `registerUserRoutes` provided contract(s) http::GET::/v1/users consumed by 1 other repo(s): own-api"
        );
        // Anchored to where the registrar WAS, like rule A's removals.
        assert_eq!(f.anchor_method, AnchorMethod::OldSideLine);
        assert_eq!(f.line, Some(2));
        // Consumer sites fold exactly like the Added/Modified path.
        assert_eq!(f.related.len(), 1);
        assert_eq!(f.related[0].file, "own-api:src/client.js");
        assert_eq!(f.related[0].name, "loadUsers");
        // No in-repo callers means nothing blocks: one warning, REVIEW.
        assert_eq!(result.verdict, ReviewVerdict::Review);
    }

    #[test]
    fn rule_c_removed_provider_without_external_consumers_no_finding() {
        // The widened gate still needs external consumers: a removed
        // provider whose contracts nobody else consumes is ordinary
        // dead-code deletion — rule A's in-repo question applies alone.
        if !git_available() {
            return;
        }
        let repos_dir = TempDir::new().unwrap();
        let (_own_dir, root, conn) = make_cross_repo_review_repo(
            repos_dir.path(),
            "users-svc",
            "payments",
            &[("src/routes.js", CROSS_REPO_ROUTES)],
        );
        // No sibling registered: the workspace resolves with zero links.
        let inputs = CrossRepoInputs {
            declared: vec!["payments".to_string()],
            repos_dir: repos_dir.path().to_path_buf(),
        };
        std::fs::write(root.join("src/routes.js"), "const app = express();\n").unwrap();

        let result = run_review(
            &conn,
            &ChangeScope::Unstaged,
            &root,
            &ReviewOptions::default(),
            Some(&inputs),
        )
        .unwrap();
        assert!(
            result.findings.iter().all(|f| f.kind != "cross-repo"),
            "no external consumers, no rule C, got: {:?}",
            result.findings
        );
        assert_eq!(result.verdict, ReviewVerdict::Approve);
    }

    #[test]
    fn rule_c_symbol_without_provider_contracts_skipped() {
        if !git_available() {
            return;
        }
        let repos_dir = TempDir::new().unwrap();
        let (_own_dir, root, conn) = make_cross_repo_review_repo(
            repos_dir.path(),
            "users-svc",
            "payments",
            &[
                ("src/routes.js", CROSS_REPO_ROUTES),
                ("src/util.js", "function util() { return 1; }\n"),
            ],
        );
        let _sib = registered_sibling(
            repos_dir.path(),
            "own-api",
            "payments",
            &[("src/client.js", SIBLING_CLIENT)],
        );
        let inputs = CrossRepoInputs {
            declared: vec!["payments".to_string()],
            repos_dir: repos_dir.path().to_path_buf(),
        };
        // Edit the contract-less symbol only.
        std::fs::write(root.join("src/util.js"), "function util() { return 2; }\n").unwrap();

        let result = run_review(
            &conn,
            &ChangeScope::Unstaged,
            &root,
            &ReviewOptions::default(),
            Some(&inputs),
        )
        .unwrap();
        assert!(
            result.findings.iter().all(|f| f.kind != "cross-repo"),
            "no provider contracts, no rule C, got: {:?}",
            result.findings
        );
    }

    #[test]
    fn rule_c_own_repo_consumer_only_no_finding() {
        // Cross-repo impact means consumers in OTHER repos. An own-repo
        // consumer of the own provider is never a link (084's contract).
        if !git_available() {
            return;
        }
        let repos_dir = TempDir::new().unwrap();
        let (_own_dir, root, conn) = make_cross_repo_review_repo(
            repos_dir.path(),
            "users-svc",
            "payments",
            &[
                ("src/routes.js", CROSS_REPO_ROUTES),
                ("src/client.js", SIBLING_CLIENT),
            ],
        );
        let inputs = CrossRepoInputs {
            declared: vec!["payments".to_string()],
            repos_dir: repos_dir.path().to_path_buf(),
        };
        std::fs::write(root.join("src/routes.js"), CROSS_REPO_ROUTES_EDITED).unwrap();

        let result = run_review(
            &conn,
            &ChangeScope::Unstaged,
            &root,
            &ReviewOptions::default(),
            Some(&inputs),
        )
        .unwrap();
        assert!(
            result.findings.iter().all(|f| f.kind != "cross-repo"),
            "own consumers are not cross-repo impact, got: {:?}",
            result.findings
        );
    }

    #[test]
    fn ac4c_rule_c_related_folds_like_blast_cross_repo_tier() {
        // Never-disagree extended to rule C: the finding's related set is
        // the same consumer folding `wonk blast` appends as its CrossRepo
        // tier — same filter, same <repo>:<file> paths, same names.
        if !git_available() {
            return;
        }
        let repos_dir = TempDir::new().unwrap();
        let (_own_dir, root, conn) = make_cross_repo_review_repo(
            repos_dir.path(),
            "users-svc",
            "payments",
            &[("src/routes.js", CROSS_REPO_ROUTES)],
        );
        let _sib = registered_sibling(
            repos_dir.path(),
            "own-api",
            "payments",
            &[("src/client.js", SIBLING_CLIENT)],
        );
        let inputs = CrossRepoInputs {
            declared: vec!["payments".to_string()],
            repos_dir: repos_dir.path().to_path_buf(),
        };
        std::fs::write(root.join("src/routes.js"), CROSS_REPO_ROUTES_EDITED).unwrap();

        let result = run_review(
            &conn,
            &ChangeScope::Unstaged,
            &root,
            &ReviewOptions::default(),
            Some(&inputs),
        )
        .unwrap();
        let finding = result
            .findings
            .iter()
            .find(|f| f.kind == "cross-repo")
            .expect("cross-repo finding");

        let provider_ids = blast::provider_contract_ids(&conn, "registerUserRoutes").unwrap();
        let consumers = blast::resolve_cross_repo_consumers(
            &root,
            &conn,
            &["payments".to_string()],
            &inputs.repos_dir,
            &provider_ids,
        )
        .unwrap();
        let folded: Vec<SymbolRef> = consumers
            .iter()
            .map(|c| SymbolRef {
                name: c.symbol.clone().unwrap_or_else(|| c.canonical_id.clone()),
                kind: crate::types::SymbolKind::Function,
                file: format!("{}:{}", c.repo, c.file),
                line: c.line,
            })
            .collect();
        assert_eq!(finding.related, folded);
    }

    // -- rule C composition: fail-soft + once-only -------------------------------

    const CROSS_REPO_ROUTES_TWO: &str = "const app = express();\nfunction registerUserRoutes() {\n  app.get('/v1/users', getUser);\n}\nfunction registerOrderRoutes() {\n  app.get('/v1/orders', getOrders);\n}\n";
    const CROSS_REPO_ROUTES_TWO_EDITED: &str = "const app = express();\nfunction registerUserRoutes() {\n  app.get('/v1/users', getUserV2);\n}\nfunction registerOrderRoutes() {\n  app.get('/v1/orders', getOrdersV2);\n}\n";

    #[test]
    fn cross_repo_resolution_failure_fails_soft_and_warns_once() {
        // A broken registry must never break the review: rules A/B keep
        // their findings, rule C degrades with exactly one warning — one
        // resolution ATTEMPT per run, so two provider symbols still warn
        // once (per-symbol resolution would warn twice).
        if !git_available() {
            return;
        }
        let repos_dir = TempDir::new().unwrap();
        let (_own_dir, root, conn) = make_cross_repo_review_repo(
            repos_dir.path(),
            "users-svc",
            "payments",
            &[("src/routes.js", CROSS_REPO_ROUTES_TWO)],
        );
        let _sib = registered_sibling(
            repos_dir.path(),
            "own-api",
            "payments",
            &[("src/client.js", SIBLING_CLIENT)],
        );
        // Break the registry after registration: the sibling's entry stays
        // discoverable (index.db exists, meta.json readable) but its index
        // is unopenable garbage — a lazy-open failure, not a skip.
        for entry in std::fs::read_dir(repos_dir.path()).unwrap().flatten() {
            let index = entry.path().join("index.db");
            if index.exists() {
                std::fs::write(&index, b"not a sqlite database").unwrap();
            }
        }
        let inputs = CrossRepoInputs {
            declared: vec!["payments".to_string()],
            repos_dir: repos_dir.path().to_path_buf(),
        };

        std::fs::write(root.join("src/routes.js"), CROSS_REPO_ROUTES_TWO_EDITED).unwrap();
        let result = run_review(
            &conn,
            &ChangeScope::Unstaged,
            &root,
            &ReviewOptions::default(),
            Some(&inputs),
        )
        .unwrap();

        let cross_warnings: Vec<_> = result
            .warnings
            .iter()
            .filter(|w| w.contains("cross-repo"))
            .collect();
        assert_eq!(
            cross_warnings.len(),
            1,
            "two provider candidates, still one warning: {:?}",
            result.warnings
        );
        assert!(
            cross_warnings[0].contains("workspace resolution failed"),
            "{}",
            cross_warnings[0]
        );
        // Fail-soft is scoped to rule C: coverage gaps still computed.
        assert_eq!(
            result
                .findings
                .iter()
                .filter(|f| f.kind == "coverage-gap")
                .count(),
            2,
            "got: {:?}",
            result.findings
        );
        assert!(result.findings.iter().all(|f| f.kind != "cross-repo"));
    }

    #[test]
    fn cross_repo_enabled_without_inputs_warns_once() {
        // The engine never re-derives the registry from $HOME: enabled rule
        // C without explicit inputs degrades with one warning, once, no
        // matter how many candidates the diff has.
        if !git_available() {
            return;
        }
        let (dir, conn) = make_review_repo(&[(
            "src/lib.rs",
            "pub fn f() -> i32 { 1 }\npub fn g() -> i32 { 2 }\n",
        )]);
        let root = dir.path();
        std::fs::write(
            root.join("src/lib.rs"),
            "pub fn f() -> i32 { 11 }\npub fn g() -> i32 { 22 }\n",
        )
        .unwrap();

        let result = run_review(
            &conn,
            &ChangeScope::Unstaged,
            root,
            &ReviewOptions::default(),
            None,
        )
        .unwrap();

        let cross: Vec<_> = result
            .warnings
            .iter()
            .filter(|w| w.contains("cross-repo"))
            .collect();
        assert_eq!(
            cross.len(),
            1,
            "two candidates, still one warning: {:?}",
            result.warnings
        );
        assert!(cross[0].contains("no cross-repo inputs"), "{}", cross[0]);
        assert_eq!(
            result
                .findings
                .iter()
                .filter(|f| f.kind == "coverage-gap")
                .count(),
            2,
            "rules A/B unaffected: {:?}",
            result.findings
        );
    }
}
