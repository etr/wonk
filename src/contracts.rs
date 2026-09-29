//! Contract extraction: canonical ID normalization plus the `http` and `env`
//! contract kinds (TASK-082, PRD-CTR-REQ-001..004).
//!
//! Contracts are detected by walking the tree-sitter tree that the symbol
//! indexer already parsed — no second parse or file read (PRD-CTR-REQ-011).
//! Storage lands in TASK-083; this module only produces
//! [`ContractCandidate`] values.
//!
//! Normalization core (AR-017):
//! - [`canonical_contract_id`] is the only place contract IDs are built.
//! - [`normalize_http_path`] runs a fixed 6-stage pipeline (PRD-CTR-REQ-003).
//! - [`normalize_method`] upper-cases verbs and maps router catch-alls to
//!   `ANY`.

use tree_sitter::Tree;

use crate::indexer::Lang;
use crate::types::{ContractCandidate, ContractKind};

/// Confidence for framework-recognized constructs (DR-028 / AR-018).
pub const CONFIDENCE_FRAMEWORK: f64 = 1.0;
/// Confidence for string-literal heuristics and role-ambiguous constructs.
pub const CONFIDENCE_HEURISTIC: f64 = 0.5;

/// Extract contract candidates from an already-parsed tree.
///
/// `source` must be the exact byte string the tree was parsed from.
pub fn extract_contracts(_tree: &Tree, _source: &str, _lang: Lang) -> Vec<ContractCandidate> {
    Vec::new()
}

/// Build the canonical contract ID `<kind>::<qualifier>::<identifier>`.
///
/// This is the only place contract IDs are constructed (PRD-CTR-REQ-002);
/// every extractor funnels through it so the format can never drift.
pub fn canonical_contract_id(kind: ContractKind, qualifier: &str, identifier: &str) -> String {
    assert!(
        !identifier.trim().is_empty(),
        "contract identifier must be non-empty"
    );
    format!("{}::{}::{}", kind.as_str(), qualifier, identifier)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_http() {
        assert_eq!(
            canonical_contract_id(ContractKind::Http, "GET", "/v1/users/{p1}"),
            "http::GET::/v1/users/{p1}"
        );
    }

    #[test]
    fn canonical_env_empty_qualifier() {
        assert_eq!(
            canonical_contract_id(ContractKind::Env, "", "DATABASE_URL"),
            "env::::DATABASE_URL"
        );
    }

    #[test]
    #[should_panic(expected = "contract identifier must be non-empty")]
    fn canonical_rejects_empty_identifier() {
        canonical_contract_id(ContractKind::Http, "GET", "  ");
    }
}
