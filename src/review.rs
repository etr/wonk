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

use crate::types::{AnchorMethod, ChangedSymbol, FileDiffHunks, Finding, ReviewVerdict, Symbol};

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
}
