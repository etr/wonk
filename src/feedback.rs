//! Usage-feedback capture (TASK-101, DR-042, PRD-FB-REQ-001..015).
//!
//! Wonk persists the ranked slate at SEARCH time (`feedback_slates`), and
//! the feedback call (`wonk_feedback` MCP tool / `wonk feedback` CLI)
//! references that persisted slate by token — so the recorded signal
//! vectors are wonk's own retained contributions, never caller-echoed and
//! never re-derived from a drifted re-search. Entries are keyed on a
//! content-anchored result identity (the review `finding_identity`
//! technique, TASK-085) that survives re-indexing; retirement is read-time
//! liveness resolution, never a write. Everything stays in the per-repo
//! index DB; there is no telemetry path (PRD-FB-REQ-004).

use sha2::{Digest, Sha256};

/// Stable result identity: SHA-256 hex of
/// `"result" \x1f file \x1f kind \x1f name \x1f fold(signature)`.
///
/// The sibling of review's `finding_identity` (TASK-085) — component-joined
/// SHA-256 with a whitespace-folded text anchor and the line number
/// structurally absent, so line shifts cannot change identity. All
/// components come from one `symbols` row, which is what makes
/// re-derivation at read time possible (`live_identities`): the anchor is
/// the symbol's stored `signature`, not the matched line. A rename, kind
/// change, signature-token change, or file move yields a different
/// identity — the material-change boundary `ChangeAnalysisDetail.
/// signature_changed` already draws (PRD-FB-REQ-005/006).
pub fn result_identity(file: &str, kind: &str, name: &str, signature: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"result");
    for part in [file, kind, name] {
        hasher.update([0x1f]);
        hasher.update(part.as_bytes());
    }
    hasher.update([0x1f]);
    hasher.update(crate::review::fold_whitespace(signature).as_bytes());
    hex(hasher)
}

/// Stable identity for a result with no owning symbol: SHA-256 hex of
/// `"result-line" \x1f file \x1f category \x1f fold(content)`.
///
/// File-level matches (a line outside every symbol span) anchor on the
/// matched line's content instead of a signature; `category` is the
/// result's ranker category. These identities never retire by drift —
/// they carry `symbol: null` so the learner can down-weight them.
pub fn line_identity(file: &str, category: &str, content: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"result-line");
    for part in [file, category] {
        hasher.update([0x1f]);
        hasher.update(part.as_bytes());
    }
    hasher.update([0x1f]);
    hasher.update(crate::review::fold_whitespace(content).as_bytes());
    hex(hasher)
}

/// Finalize a hasher as lowercase hex.
fn hex(hasher: Sha256) -> String {
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- result_identity -------------------------------------------------------

    #[test]
    fn result_identity_is_stable_across_calls() {
        let a = result_identity(
            "src/auth.rs",
            "function",
            "handle_login",
            "fn handle_login(u: &User) -> Result<Token>",
        );
        let b = result_identity(
            "src/auth.rs",
            "function",
            "handle_login",
            "fn handle_login(u: &User) -> Result<Token>",
        );
        assert_eq!(a, b);
        assert_eq!(a.len(), 64, "SHA-256 hex: {a}");
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn result_identity_folds_signature_whitespace() {
        let tight = result_identity("a.rs", "function", "f", "fn f(x: i32) -> i32 {");
        let reflowed = result_identity("a.rs", "function", "f", "fn   f(x: i32)\t->  i32 {");
        assert_eq!(tight, reflowed, "signature reflow must not change identity");
    }

    #[test]
    fn result_identity_differs_per_component() {
        let base = result_identity("a.rs", "function", "f", "fn f(x: i32)");
        assert_ne!(
            result_identity("b.rs", "function", "f", "fn f(x: i32)"),
            base,
            "file change must change identity"
        );
        assert_ne!(
            result_identity("a.rs", "method", "f", "fn f(x: i32)"),
            base,
            "kind change must change identity"
        );
        assert_ne!(
            result_identity("a.rs", "function", "g", "fn f(x: i32)"),
            base,
            "name change must change identity"
        );
        assert_ne!(
            result_identity("a.rs", "function", "f", "fn f(y: i32)"),
            base,
            "signature token change must change identity"
        );
    }

    #[test]
    fn result_identity_is_distinct_from_line_identity() {
        let result = result_identity("a.rs", "function", "f", "fn f(x: i32)");
        let line = line_identity("a.rs", "definition", "fn f(x: i32)");
        assert_ne!(result, line);
    }

    // -- line_identity ----------------------------------------------------------

    #[test]
    fn line_identity_differs_per_category_and_content() {
        let base = line_identity("a.rs", "definition", "let x = compute();");
        assert_ne!(
            line_identity("a.rs", "call", "let x = compute();"),
            base,
            "category change must change identity"
        );
        assert_ne!(
            line_identity("a.rs", "definition", "let y = compute();"),
            base,
            "content change must change identity"
        );
        assert_eq!(
            base,
            line_identity("a.rs", "definition", "let  x =  compute();")
        );
    }
}
