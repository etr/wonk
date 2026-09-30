//! Diff-scoped review engine (TASK-085).
//!
//! Pure composition over existing primitives: review never computes impact
//! of its own — every affected-symbol set is [`crate::blast::analyze_blast`]'s
//! own output, so `wonk review` and `wonk blast` cannot disagree. Findings
//! are emitted only (DR-035): no forge posting, no auto-fix.
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
}

impl Default for ReviewOptions {
    fn default() -> Self {
        Self {
            breaking_change: true,
            coverage_gap: true,
            cross_repo: true,
            reach_enabled: true,
            depth: blast::DEFAULT_DEPTH,
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
}

/// The outcome of reviewing one diff scope.
#[derive(Debug, Clone)]
pub struct ReviewResult {
    /// The scope that was reviewed.
    pub scope: ChangeScope,
    /// All findings, sorted by severity (desc), file, line, rule.
    pub findings: Vec<Finding>,
    /// Mechanically derived from `findings` by [`derive_verdict`].
    pub verdict: ReviewVerdict,
    /// Non-fatal problems (e.g. a per-symbol blast failure) — findings the
    /// engine could not compute are never silently dropped.
    pub warnings: Vec<String>,
}

// ---------------------------------------------------------------------------
// Finding identity (PRD-REV-REQ-013)
// ---------------------------------------------------------------------------

/// Collapse every whitespace run to a single space and trim, so re-indenting
/// or reflowing a line leaves the identity untouched while any token change
/// still alters it (AR-031).
fn fold_whitespace(text: &str) -> String {
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

// ---------------------------------------------------------------------------
// Rule family A — breaking change (PRD-REV-REQ-006)
// ---------------------------------------------------------------------------

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
    removed: &HashSet<(String, crate::types::SymbolKind)>,
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
                .filter(|s| !removed.contains(&(s.name.clone(), s.kind)))
                .collect()
        })
        .unwrap_or_default();
    if surviving.is_empty() {
        return None;
    }

    let names: Vec<&str> = surviving.iter().map(|s| s.name.as_str()).collect();
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
        kind: "breaking-change".into(),
        rule: rule.into(),
        message,
        identity: finding_identity(rule, "breaking-change", &cs.file, &cs.name, None),
        related: surviving
            .iter()
            .map(|s| SymbolRef {
                name: s.name.clone(),
                kind: s.kind,
                file: s.file.clone(),
                line: s.line,
            })
            .collect(),
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
        kind: "coverage-gap".into(),
        rule: rule.into(),
        message,
        identity: finding_identity(rule, "coverage-gap", &cs.file, &cs.name, None),
        related: context
            .tiers
            .iter()
            .flat_map(|t| t.symbols.iter())
            .map(|s| SymbolRef {
                name: s.name.clone(),
                kind: s.kind,
                file: s.file.clone(),
                line: s.line,
            })
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
        kind: "cross-repo".into(),
        rule: rule.into(),
        message,
        identity: finding_identity(rule, "cross-repo", &cs.file, &cs.name, None),
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

    // Callers removed in this same diff are dead code, not breakage.
    let removed: HashSet<(String, crate::types::SymbolKind)> = detail
        .analysis
        .changed_symbols
        .iter()
        .filter(|c| c.change_type == crate::types::ChangeType::Removed)
        .map(|c| (c.name.clone(), c.kind))
        .collect();

    // Per-file cache of current-file symbols for tier-3 re-resolution.
    let mut current_cache: HashMap<String, Option<Vec<Symbol>>> = HashMap::new();

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
        let (line, anchor_method) = resolve_anchor(cs, detail.hunks.get(&cs.file), current_symbols);

        if rule_a_candidate
            && let Some(ref context) = context
            && let Some(finding) = rule_breaking_change(cs, context, &removed, line, anchor_method)
        {
            findings.push(finding);
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
                        findings.push(finding);
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
                            findings.push(finding);
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

    findings.sort_by(|a, b| {
        b.severity
            .cmp(&a.severity)
            .then_with(|| a.file.cmp(&b.file))
            .then_with(|| a.line.cmp(&b.line))
            .then_with(|| a.rule.cmp(&b.rule))
    });

    let verdict = derive_verdict(&findings);

    Ok(ReviewResult {
        scope: scope.clone(),
        findings,
        verdict,
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
        assert_eq!(
            f.message,
            "function `f` changed but no test file appears in its blast radius (1 affected symbol(s), none in tests)"
        );
        // Related context is the canonical (tests-excluded) blast radius.
        assert_eq!(f.related.len(), 1);
        assert_eq!(f.related[0].name, "g");
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
            .map(|s| SymbolRef {
                name: s.name.clone(),
                kind: s.kind,
                file: s.file.clone(),
                line: s.line,
            })
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
            .map(|t| {
                t.symbols
                    .iter()
                    .map(|s| SymbolRef {
                        name: s.name.clone(),
                        kind: s.kind,
                        file: s.file.clone(),
                        line: s.line,
                    })
                    .collect()
            })
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
