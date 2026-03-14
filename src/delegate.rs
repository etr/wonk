//! LLM delegate — RAG pipeline over the code index.
//!
//! Accepts a natural-language question, gathers relevant structural context
//! from the index (via semantic search + symbol metadata), constructs a
//! grounded prompt, and delegates to Ollama for an answer.

use std::fmt::Write as _;
use std::path::Path;

use rusqlite::Connection;

use crate::config::LlmConfig;
use crate::errors::LlmError;
use crate::types::SemanticResult;

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Result of a delegate query.
#[derive(Debug, Clone)]
pub struct DelegateResult {
    /// The LLM-generated answer.
    pub answer: String,
    /// Symbols used as context (for attribution).
    pub context_symbols: Vec<ContextSymbol>,
}

/// A symbol included in the delegate prompt context.
#[derive(Debug, Clone)]
pub struct ContextSymbol {
    pub name: String,
    pub kind: String,
    pub file: String,
    pub line: usize,
    pub similarity: f32,
}

/// Maximum number of semantic search results to use as context.
const MAX_CONTEXT_SYMBOLS: usize = 10;

/// Maximum source lines to include per symbol in the prompt.
const MAX_SOURCE_LINES: usize = 40;

/// Run the delegate pipeline: semantic search → gather context → LLM generate.
///
/// `scope` optionally restricts context to symbols under a path prefix.
pub fn delegate(
    conn: &Connection,
    repo_root: &Path,
    config: &LlmConfig,
    question: &str,
    scope: Option<&str>,
) -> Result<DelegateResult, LlmError> {
    // 1. Semantic search to find relevant symbols.
    let semantic_results = semantic_search_for_context(conn, question)?;

    if semantic_results.is_empty() {
        // No embeddings — fall back to a context-free answer with a caveat.
        let prompt = build_prompt_no_context(question);
        let answer = crate::llm::generate(config, &prompt)?;
        return Ok(DelegateResult {
            answer,
            context_symbols: Vec::new(),
        });
    }

    // 2. Filter by scope if provided.
    let filtered: Vec<&SemanticResult> = if let Some(prefix) = scope {
        semantic_results
            .iter()
            .filter(|sr| sr.file.starts_with(prefix))
            .take(MAX_CONTEXT_SYMBOLS)
            .collect()
    } else {
        semantic_results.iter().take(MAX_CONTEXT_SYMBOLS).collect()
    };

    // 3. Gather structural context for each symbol.
    let context_entries = gather_context(conn, repo_root, &filtered);

    // 4. Build the delegate prompt.
    let prompt = build_delegate_prompt(question, &context_entries);

    // 5. Generate answer via Ollama.
    let answer = crate::llm::generate(config, &prompt)?;

    let context_symbols = filtered
        .iter()
        .map(|sr| ContextSymbol {
            name: sr.symbol_name.clone(),
            kind: sr.symbol_kind.to_string(),
            file: sr.file.clone(),
            line: sr.line,
            similarity: sr.similarity_score,
        })
        .collect();

    Ok(DelegateResult {
        answer,
        context_symbols,
    })
}

// ---------------------------------------------------------------------------
// Semantic search
// ---------------------------------------------------------------------------

fn semantic_search_for_context(
    conn: &Connection,
    question: &str,
) -> Result<Vec<SemanticResult>, LlmError> {
    let embeddings = crate::embedding::load_all_embeddings(conn)
        .map_err(|e| LlmError::QueryFailed(format!("failed to load embeddings: {e}")))?;

    if embeddings.is_empty() {
        return Ok(Vec::new());
    }

    let client = crate::embedding::OllamaClient::new();
    let mut query_vec = client
        .embed_single(question)
        .map_err(|_| LlmError::OllamaUnreachable)?;
    crate::embedding::normalize(&mut query_vec);

    let scored = crate::semantic::semantic_search(&query_vec, &embeddings, MAX_CONTEXT_SYMBOLS * 2);
    let results = crate::semantic::resolve_results(conn, &scored)
        .map_err(|e| LlmError::QueryFailed(format!("result resolution failed: {e}")))?;

    Ok(results)
}

// ---------------------------------------------------------------------------
// Context gathering
// ---------------------------------------------------------------------------

struct ContextEntry {
    name: String,
    kind: String,
    file: String,
    line: usize,
    signature: Option<String>,
    source_snippet: Option<String>,
    callers: Vec<String>,
    callees: Vec<String>,
}

fn gather_context(
    conn: &Connection,
    repo_root: &Path,
    results: &[&SemanticResult],
) -> Vec<ContextEntry> {
    let mut entries = Vec::new();

    for sr in results {
        let signature = query_signature(conn, sr.symbol_id);
        let source_snippet = read_source_snippet(conn, repo_root, sr);
        let callers = query_callers(conn, sr.symbol_id);
        let callees = query_callees(conn, sr.symbol_id);

        entries.push(ContextEntry {
            name: sr.symbol_name.clone(),
            kind: sr.symbol_kind.to_string(),
            file: sr.file.clone(),
            line: sr.line,
            signature,
            source_snippet,
            callers,
            callees,
        });
    }

    entries
}

fn query_signature(conn: &Connection, symbol_id: i64) -> Option<String> {
    conn.query_row(
        "SELECT signature FROM symbols WHERE id = ?1",
        rusqlite::params![symbol_id],
        |row| row.get::<_, String>(0),
    )
    .ok()
}

fn read_source_snippet(conn: &Connection, repo_root: &Path, sr: &SemanticResult) -> Option<String> {
    let (line, end_line): (i64, Option<i64>) = conn
        .query_row(
            "SELECT line, end_line FROM symbols WHERE id = ?1",
            rusqlite::params![sr.symbol_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .ok()?;

    let start = line as usize;
    let end = end_line.unwrap_or(line) as usize;

    // Cap the snippet length.
    let end = end.min(start + MAX_SOURCE_LINES);

    let file_path = repo_root.join(&sr.file);
    let content = std::fs::read_to_string(&file_path).ok()?;
    let lines: Vec<&str> = content.lines().collect();

    if start == 0 || start > lines.len() {
        return None;
    }

    let slice_start = start.saturating_sub(1);
    let slice_end = end.min(lines.len());
    Some(lines[slice_start..slice_end].join("\n"))
}

fn query_callers(conn: &Connection, symbol_id: i64) -> Vec<String> {
    let mut stmt = conn
        .prepare_cached(
            "SELECT DISTINCT s.name FROM symbols s \
             JOIN refs r ON r.file = s.file \
             AND r.line >= s.line AND (s.end_line IS NULL OR r.line <= s.end_line) \
             WHERE r.target_name = (SELECT name FROM symbols WHERE id = ?1) \
             AND s.id != ?1 \
             LIMIT 5",
        )
        .ok();

    match stmt {
        Some(ref mut s) => s
            .query_map(rusqlite::params![symbol_id], |row| row.get::<_, String>(0))
            .ok()
            .map(|rows| rows.filter_map(|r| r.ok()).collect())
            .unwrap_or_default(),
        None => Vec::new(),
    }
}

fn query_callees(conn: &Connection, symbol_id: i64) -> Vec<String> {
    let mut stmt = conn
        .prepare_cached(
            "SELECT DISTINCT r.target_name FROM refs r \
             JOIN symbols s ON s.id = ?1 \
             WHERE r.file = s.file \
             AND r.line >= s.line AND (s.end_line IS NULL OR r.line <= s.end_line) \
             LIMIT 5",
        )
        .ok();

    match stmt {
        Some(ref mut s) => s
            .query_map(rusqlite::params![symbol_id], |row| row.get::<_, String>(0))
            .ok()
            .map(|rows| rows.filter_map(|r| r.ok()).collect())
            .unwrap_or_default(),
        None => Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// Prompt construction
// ---------------------------------------------------------------------------

/// Strip newlines and control characters from code-derived strings.
fn sanitize(s: &str) -> String {
    s.replace(['\n', '\r', '\0'], " ")
}

fn build_delegate_prompt(question: &str, context: &[ContextEntry]) -> String {
    let mut prompt = String::with_capacity(8192);

    prompt.push_str(
        "You are a code assistant. Answer the following question about the codebase \
         using ONLY the context provided below. If the context is insufficient, say so.\n\n",
    );

    prompt.push_str("=== CODEBASE CONTEXT ===\n\n");

    for entry in context {
        writeln!(
            prompt,
            "--- {} ({}) in {} ---",
            entry.name, entry.kind, entry.file
        )
        .unwrap();

        if let Some(ref sig) = entry.signature {
            writeln!(prompt, "Signature: {}", sanitize(sig)).unwrap();
        }

        if !entry.callers.is_empty() {
            writeln!(prompt, "Called by: {}", entry.callers.join(", ")).unwrap();
        }

        if !entry.callees.is_empty() {
            writeln!(prompt, "Calls: {}", entry.callees.join(", ")).unwrap();
        }

        if let Some(ref src) = entry.source_snippet {
            writeln!(prompt, "Source (line {}):", entry.line).unwrap();
            writeln!(prompt, "```").unwrap();
            writeln!(prompt, "{src}").unwrap();
            writeln!(prompt, "```").unwrap();
        }

        prompt.push('\n');
    }

    prompt.push_str("=== QUESTION ===\n");
    writeln!(prompt, "{question}").unwrap();

    prompt
}

fn build_prompt_no_context(question: &str) -> String {
    format!(
        "You are a code assistant. Answer the following question about the codebase. \
         Note: no indexed context was available, so answer based on general knowledge only.\n\n\
         Question: {question}"
    )
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_delegate_prompt_includes_question() {
        let entries = vec![ContextEntry {
            name: "processPayment".into(),
            kind: "function".into(),
            file: "src/billing.ts".into(),
            line: 42,
            signature: Some("function processPayment(amount: number): void".into()),
            source_snippet: Some(
                "function processPayment(amount: number): void {\n  // ...\n}".into(),
            ),
            callers: vec!["handleCheckout".into()],
            callees: vec!["chargeCard".into()],
        }];

        let prompt = build_delegate_prompt("How does payment processing work?", &entries);

        assert!(prompt.contains("How does payment processing work?"));
        assert!(prompt.contains("processPayment"));
        assert!(prompt.contains("function"));
        assert!(prompt.contains("src/billing.ts"));
        assert!(prompt.contains("handleCheckout"));
        assert!(prompt.contains("chargeCard"));
        assert!(prompt.contains("```"));
    }

    #[test]
    fn build_delegate_prompt_empty_context() {
        let prompt = build_delegate_prompt("What does this code do?", &[]);
        assert!(prompt.contains("What does this code do?"));
        assert!(prompt.contains("CODEBASE CONTEXT"));
    }

    #[test]
    fn build_prompt_no_context_includes_question() {
        let prompt = build_prompt_no_context("How does auth work?");
        assert!(prompt.contains("How does auth work?"));
        assert!(prompt.contains("no indexed context"));
    }

    #[test]
    fn sanitize_strips_control_chars() {
        assert_eq!(sanitize("fn foo()\nbar"), "fn foo() bar");
    }
}
