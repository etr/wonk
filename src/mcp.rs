//! MCP (Model Context Protocol) server over stdio.
//!
//! Implements a JSON-RPC 2.0 server that exposes wonk's query capabilities
//! as MCP tools. Designed for use with AI coding assistants (Claude Code, etc.)
//! via the `wonk mcp serve` command.
//!
//! Transport: NDJSON over stdin/stdout. No async runtime required.

use std::collections::HashMap;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use anyhow::Result;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::budget::TokenBudget;
use crate::db;
use crate::output::{
    CallPathHopOutput, CalleeOutput, CallerOutput, DepOutput, OutputFormat, RefOutput,
    SearchOutput, ShowOutput, SignatureOutput, SummaryOutput, SymbolOutput,
};
use crate::pipeline;
use crate::progress::Progress;
use crate::ranker;
use crate::router::QueryRouter;
use crate::search;
use crate::types::Symbol;

// ---------------------------------------------------------------------------
// JSON-RPC 2.0 types
// ---------------------------------------------------------------------------

const JSONRPC_VERSION: &str = "2.0";
const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
enum RequestId {
    Number(i64),
    Str(String),
}

#[derive(Debug, Serialize)]
struct Response {
    jsonrpc: &'static str,
    id: RequestId,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<JsonRpcError>,
}

#[derive(Debug, Serialize)]
struct JsonRpcError {
    code: i64,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<Value>,
}

impl Response {
    fn success(id: RequestId, result: Value) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION,
            id,
            result: Some(result),
            error: None,
        }
    }

    fn error(id: RequestId, code: i64, message: impl Into<String>) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION,
            id,
            result: None,
            error: Some(JsonRpcError {
                code,
                message: message.into(),
                data: None,
            }),
        }
    }
}

// ---------------------------------------------------------------------------
// MCP protocol types
// ---------------------------------------------------------------------------

const PROTOCOL_VERSION: &str = "2025-11-25";

#[derive(Debug, Serialize)]
struct InitializeResult {
    #[serde(rename = "protocolVersion")]
    protocol_version: &'static str,
    capabilities: ServerCapabilities,
    #[serde(rename = "serverInfo")]
    server_info: ServerInfo,
    #[serde(skip_serializing_if = "Option::is_none")]
    instructions: Option<&'static str>,
}

#[derive(Debug, Serialize)]
struct ServerCapabilities {
    tools: ToolsCapability,
}

#[derive(Debug, Serialize)]
struct ToolsCapability {}

#[derive(Debug, Serialize)]
struct ServerInfo {
    name: &'static str,
    version: &'static str,
}

#[derive(Debug, Clone, Serialize)]
struct Tool {
    name: &'static str,
    description: &'static str,
    #[serde(rename = "inputSchema")]
    input_schema: Value,
}

#[derive(Debug, Deserialize)]
struct CallToolParams {
    name: String,
    #[serde(default)]
    arguments: Value,
}

#[derive(Debug, Serialize)]
struct CallToolResult {
    content: Vec<Content>,
    #[serde(rename = "isError", skip_serializing_if = "std::ops::Not::not")]
    is_error: bool,
}

#[derive(Debug, Serialize)]
struct Content {
    #[serde(rename = "type")]
    type_: &'static str,
    text: String,
}

impl Content {
    fn text(text: String) -> Self {
        Self {
            type_: "text",
            text,
        }
    }
}

impl CallToolResult {
    fn success(text: String) -> Self {
        Self {
            content: vec![Content::text(text)],
            is_error: false,
        }
    }

    fn error(message: String) -> Self {
        Self {
            content: vec![Content::text(message)],
            is_error: true,
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Extract a required string parameter from JSON args, returning a
/// `CallToolResult::error` on missing.
fn require_str(args: &Value, key: &str) -> Result<String, CallToolResult> {
    args.get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| CallToolResult::error(format!("missing required parameter: {key}")))
}

/// Serialize any `Serialize` value into a `CallToolResult` using the given format.
fn format_result<T: Serialize>(data: &T, format: OutputFormat) -> CallToolResult {
    let text: Result<String, String> = match format {
        OutputFormat::Json | OutputFormat::Grep => {
            serde_json::to_string_pretty(data).map_err(|e| e.to_string())
        }
        OutputFormat::Toon => serde_toon2::to_string(data).map_err(|e| e.to_string()),
    };
    match text {
        Ok(s) => CallToolResult::success(s),
        Err(_) => CallToolResult::error("output formatting failed".into()),
    }
}

/// Extract the output format from MCP tool args (defaults to JSON).
fn extract_format(args: &Value) -> OutputFormat {
    args.get("format")
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse().ok())
        .unwrap_or(OutputFormat::Json)
}

/// Clamp a confidence value to `[0.0, 1.0]`, mapping NaN/Inf to 0.0.
fn clamp_confidence(c: f64) -> f64 {
    if c.is_nan() || c.is_infinite() {
        0.0
    } else {
        c.clamp(0.0, 1.0)
    }
}

/// Generate diagnostic hints when a show query returns 0 results.
fn empty_show_hints(
    conn: &Connection,
    name: &str,
    file_filter: Option<&str>,
    kind_filter: Option<&str>,
) -> Vec<String> {
    let mut hints = Vec::new();

    // Fuzzy match: find symbols whose name contains the query substring.
    let like_pattern = format!("%{}%", name.replace('%', "\\%").replace('_', "\\_"));
    let mut near: Vec<String> = conn
        .prepare(
            "SELECT DISTINCT name FROM symbols WHERE name LIKE ?1 ESCAPE '\\' ORDER BY length(name) LIMIT 5",
        )
        .ok()
        .and_then(|mut stmt| {
            stmt.query_map(rusqlite::params![like_pattern], |row| row.get(0))
                .ok()
                .map(|rows| rows.flatten().collect())
        })
        .unwrap_or_default();

    // Remove the exact name if it appears (it matched by LIKE but returned 0 with filters).
    near.retain(|n| n != name);

    if !near.is_empty() {
        hints.push(format!(
            "No exact match for '{}'. Similar symbols: {}",
            name,
            near.join(", ")
        ));
    } else {
        hints.push(format!("No symbol matching '{}' found in the index.", name));
    }

    if file_filter.is_some() {
        hints.push("Try without the file filter for broader results.".into());
    }
    if kind_filter.is_some() {
        hints.push("Try without the kind filter for broader results.".into());
    }
    hints.push("Use wonk_search for text-based search if the symbol name is uncertain.".into());
    hints
}

/// Convert a `Symbol` to the serializable `SymbolOutput`.
fn symbol_to_output(sym: &Symbol) -> SymbolOutput {
    SymbolOutput {
        name: sym.name.clone(),
        kind: sym.kind.to_string(),
        file: sym.file.clone(),
        line: sym.line,
        col: sym.col,
        end_line: sym.end_line,
        scope: sym.scope.clone(),
        signature: sym.signature.clone(),
        language: sym.language.clone(),
    }
}

/// Enrich a reference context line with ±1 surrounding lines from the source file.
/// Falls back to the original context if the file can't be read.
fn enrich_context(repo_root: &Path, file: &str, line: usize, original: &str) -> String {
    if line == 0 {
        return original.to_string();
    }
    let path = repo_root.join(file);
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return original.to_string(),
    };
    let lines: Vec<&str> = content.lines().collect();
    let idx = line.saturating_sub(1); // 1-based to 0-based
    let start = idx.saturating_sub(1);
    let end = (idx + 2).min(lines.len()); // exclusive, ±1 line
    lines[start..end]
        .iter()
        .enumerate()
        .map(|(i, l)| format!("{}: {}", start + i + 1, l))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Validate that a path is within the repo root, returning a `CallToolResult::error`
/// if the path escapes the repository boundary.
fn validate_path(path: &Path, repo_root: &Path) -> Result<PathBuf, CallToolResult> {
    // Reject absolute paths — all paths must be relative to repo_root.
    if path.is_absolute() {
        return Err(CallToolResult::error(
            "absolute paths are not allowed".into(),
        ));
    }
    let resolved = repo_root.join(path);
    // Use canonicalize on the parent for non-existent files.
    let canonical: io::Result<PathBuf> = resolved.canonicalize().or_else(|_| {
        let parent = resolved
            .parent()
            .and_then(|p| p.canonicalize().ok())
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "path not found"))?;
        let name = resolved
            .file_name()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid path"))?;
        Ok(parent.join(name))
    });
    let root_canonical = repo_root
        .canonicalize()
        .map_err(|_| CallToolResult::error("repository path cannot be validated".into()))?;
    match canonical {
        Ok(p) => {
            if p.starts_with(&root_canonical) {
                Ok(p)
            } else {
                Err(CallToolResult::error(
                    "path is outside the repository".into(),
                ))
            }
        }
        Err(_) => Err(CallToolResult::error(
            "path is outside the repository".into(),
        )),
    }
}

// ---------------------------------------------------------------------------
// Tool definitions
// ---------------------------------------------------------------------------

fn tool_definitions() -> &'static Vec<Tool> {
    static TOOLS: OnceLock<Vec<Tool>> = OnceLock::new();
    TOOLS.get_or_init(|| {
        let repo_prop = serde_json::json!({
            "type": "string",
            "description": "Target repository name (last path component). Omit to use the working directory repo."
        });

        let mut tools = vec![
            Tool {
                name: "wonk_search",
                description: "Keyword/regex code search (ripgrep) — returns definitions first, deduplicates re-exports. Use single keywords or regex patterns like 'handleError|onError'. Do NOT use natural language sentences — use wonk_ask for semantic queries instead.",
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "query": {
                            "type": "string",
                            "description": "Search pattern — literal keyword or regex (if regex=true). NOT natural language."
                        },
                        "regex": {
                            "type": "boolean",
                            "description": "Treat query as a regular expression",
                            "default": false
                        },
                        "case_insensitive": {
                            "type": "boolean",
                            "description": "Case-insensitive search",
                            "default": false
                        },
                        "paths": {
                            "type": "array",
                            "items": { "type": "string" },
                            "description": "Restrict search to these file paths"
                        },
                        "budget": {
                            "type": "integer",
                            "description": "Limit output to approximately N tokens"
                        },
                        "format": {
                            "type": "string",
                            "enum": ["json", "toon"],
                            "description": "Output format (default: json)",
                            "default": "json"
                        }
                    },
                    "required": ["query"]
                }),
            },
            Tool {
                name: "wonk_sym",
                description: "Find symbol definitions by name. Returns kind, file, line, and signature. Faster and more precise than Grep for 'where is X defined' questions.",
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "name": {
                            "type": "string",
                            "description": "Symbol name to look up"
                        },
                        "kind": {
                            "type": "string",
                            "description": "Filter by symbol kind (function, method, class, struct, interface, enum, trait, type_alias, constant, variable, module)"
                        },
                        "file": {
                            "type": "string",
                            "description": "Restrict results to a specific file path (substring match)"
                        },
                        "exact": {
                            "type": "boolean",
                            "description": "Require exact name match",
                            "default": false
                        },
                        "format": {
                            "type": "string",
                            "enum": ["json", "toon"],
                            "description": "Output format (default: json)",
                            "default": "json"
                        }
                    },
                    "required": ["name"]
                }),
            },
            Tool {
                name: "wonk_ref",
                description: "Find all references (call sites and imports) of a symbol. Use output='files' when you only need file names, not per-reference details.",
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "name": {
                            "type": "string",
                            "description": "Symbol name to find references for"
                        },
                        "paths": {
                            "type": "array",
                            "items": { "type": "string" },
                            "description": "Restrict search to these file paths"
                        },
                        "budget": {
                            "type": "integer",
                            "description": "Limit output to approximately N tokens"
                        },
                        "output": {
                            "type": "string",
                            "enum": ["full", "files"],
                            "description": "Use 'files' for just unique file paths (like grep --files-with-matches). Default: full.",
                            "default": "full"
                        },
                        "format": {
                            "type": "string",
                            "enum": ["json", "toon"],
                            "description": "Output format (default: json)",
                            "default": "json"
                        }
                    },
                    "required": ["name"]
                }),
            },
            Tool {
                name: "wonk_sig",
                description: "Quick: show only function/method signatures (no bodies). Fastest way to check a function's type.",
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "name": {
                            "type": "string",
                            "description": "Function or method name"
                        },
                        "format": {
                            "type": "string",
                            "enum": ["json", "toon"],
                            "description": "Output format (default: json)",
                            "default": "json"
                        }
                    },
                    "required": ["name"]
                }),
            },
            Tool {
                name: "wonk_deps",
                description: "Show files imported/used by a file. Use for 'what does this file depend on' questions.",
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "file": {
                            "type": "string",
                            "description": "File to show dependencies for"
                        },
                        "format": {
                            "type": "string",
                            "enum": ["json", "toon"],
                            "description": "Output format (default: json)",
                            "default": "json"
                        }
                    },
                    "required": ["file"]
                }),
            },
            Tool {
                name: "wonk_rdeps",
                description: "Show files that depend on a given file.",
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "file": {
                            "type": "string",
                            "description": "File to show reverse dependencies for"
                        },
                        "format": {
                            "type": "string",
                            "enum": ["json", "toon"],
                            "description": "Output format (default: json)",
                            "default": "json"
                        }
                    },
                    "required": ["file"]
                }),
            },
            Tool {
                name: "wonk_status",
                description: "Show index status: file/symbol/reference/embedding counts and Ollama reachability.",
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "format": {
                            "type": "string",
                            "enum": ["json", "toon"],
                            "description": "Output format (default: json)",
                            "default": "json"
                        }
                    }
                }),
            },
            Tool {
                name: "wonk_init",
                description: "Initialize or rebuild the index and embeddings for the current repository.",
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "local": {
                            "type": "boolean",
                            "description": "Use a local (project-specific) index instead of the shared index",
                            "default": false
                        },
                        "format": {
                            "type": "string",
                            "enum": ["json", "toon"],
                            "description": "Output format (default: json)",
                            "default": "json"
                        }
                    }
                }),
            },
            Tool {
                name: "wonk_show",
                description: "Read source bodies of named symbols. Accepts comma-separated names for batch lookup (e.g. 'main,parse,validate'). More targeted than Read — finds the symbol across files without needing the file path. Large containers auto-fallback to shallow mode when they exceed the budget. Returns complete symbol source code — you do not need to Read the same file afterward.",
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "name": {
                            "type": "string",
                            "description": "Symbol name to look up"
                        },
                        "kind": {
                            "type": "string",
                            "description": "Filter by symbol kind (function, method, class, struct, interface, enum, trait, type_alias, constant, variable, module)"
                        },
                        "file": {
                            "type": "string",
                            "description": "Restrict results to a specific file path (substring match)"
                        },
                        "exact": {
                            "type": "boolean",
                            "description": "Require exact name match",
                            "default": false
                        },
                        "shallow": {
                            "type": "boolean",
                            "description": "Show container types in shallow mode (signature + child signatures only)",
                            "default": false
                        },
                        "budget": {
                            "type": "integer",
                            "description": "Limit output to approximately N tokens"
                        },
                        "format": {
                            "type": "string",
                            "enum": ["json", "toon"],
                            "description": "Output format (default: json)",
                            "default": "json"
                        }
                    },
                    "required": ["name"]
                }),
            },
            Tool {
                name: "wonk_callers",
                description: "Who calls this symbol? depth=1 for direct callers, depth=2+ for transitive.",
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "name": {
                            "type": "string",
                            "description": "Symbol name to find callers for"
                        },
                        "depth": {
                            "type": "integer",
                            "description": "Transitive expansion depth (default: 1 = direct callers, max: 10)",
                            "default": 1
                        },
                        "min_confidence": {
                            "type": "number",
                            "description": "Minimum edge confidence (0.0-1.0) to include in results"
                        },
                        "budget": {
                            "type": "integer",
                            "description": "Limit output to approximately N tokens"
                        },
                        "format": {
                            "type": "string",
                            "enum": ["json", "toon"],
                            "description": "Output format (default: json)",
                            "default": "json"
                        }
                    },
                    "required": ["name"]
                }),
            },
            Tool {
                name: "wonk_callees",
                description: "What does this symbol call? depth=1 for direct callees, depth=2+ for transitive.",
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "name": {
                            "type": "string",
                            "description": "Symbol name to find callees for"
                        },
                        "depth": {
                            "type": "integer",
                            "description": "Transitive expansion depth (default: 1 = direct callees, max: 10)",
                            "default": 1
                        },
                        "min_confidence": {
                            "type": "number",
                            "description": "Minimum edge confidence (0.0-1.0) to include in results"
                        },
                        "budget": {
                            "type": "integer",
                            "description": "Limit output to approximately N tokens"
                        },
                        "format": {
                            "type": "string",
                            "enum": ["json", "toon"],
                            "description": "Output format (default: json)",
                            "default": "json"
                        }
                    },
                    "required": ["name"]
                }),
            },
            Tool {
                name: "wonk_callpath",
                description: "Trace how symbol A reaches symbol B. Use for 'how does X call Y' questions — returns the call chain (e.g. main → parse → validate).",
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "from": {
                            "type": "string",
                            "description": "Source symbol name (start of call chain)"
                        },
                        "to": {
                            "type": "string",
                            "description": "Target symbol name (end of call chain)"
                        },
                        "min_confidence": {
                            "type": "number",
                            "description": "Minimum edge confidence (0.0-1.0) to include in traversal"
                        },
                        "format": {
                            "type": "string",
                            "enum": ["json", "toon"],
                            "description": "Output format (default: json)",
                            "default": "json"
                        }
                    },
                    "required": ["from", "to"]
                }),
            },
            Tool {
                name: "wonk_summary",
                description: "Architecture overview — returns file list, symbol definitions with doc comments, and import edges for a path. Compact top-level types + functions. For 'what modules exist' or 'explain this crate': pass the directory path with depth=1.",
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Path to summarize (file or directory, relative to repo root)"
                        },
                        "depth": {
                            "type": "integer",
                            "description": "Recursion depth for child summaries (0 = target only, default: 0)",
                            "default": 0
                        },
                        "recursive": {
                            "type": "boolean",
                            "description": "Show full recursive hierarchy (unlimited depth)",
                            "default": false
                        },
                        "budget": {
                            "type": "integer",
                            "description": "Limit output to approximately N tokens"
                        },
                        "format": {
                            "type": "string",
                            "enum": ["json", "toon"],
                            "description": "Output format (default: json)",
                            "default": "json"
                        }
                    },
                    "required": ["path"]
                }),
            },
            Tool {
                name: "wonk_flows",
                description: "Trace execution from an entry point through callees. Use for 'how does this flow work' questions. Omit entry to list all detected entry points.",
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "entry": {
                            "type": "string",
                            "description": "Entry point function name to trace (omit to list all detected entry points)"
                        },
                        "from": {
                            "type": "string",
                            "description": "Restrict entry point detection to symbols in this file"
                        },
                        "depth": {
                            "type": "integer",
                            "description": "Maximum BFS traversal depth (default: 10, max: 20)",
                            "default": 10
                        },
                        "branching": {
                            "type": "integer",
                            "description": "Maximum callees to follow per symbol (default: 4)",
                            "default": 4
                        },
                        "min_confidence": {
                            "type": "number",
                            "description": "Minimum edge confidence (0.0-1.0) to include in traversal"
                        },
                        "budget": {
                            "type": "integer",
                            "description": "Limit output to approximately N tokens"
                        },
                        "format": {
                            "type": "string",
                            "enum": ["json", "toon"],
                            "description": "Output format (default: json)",
                            "default": "json"
                        }
                    }
                }),
            },
            Tool {
                name: "wonk_blast",
                description: "Analyze blast radius of changing a symbol. Groups affected symbols by severity tier.",
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "symbol": {
                            "type": "string",
                            "description": "Symbol name to analyze blast radius for"
                        },
                        "direction": {
                            "type": "string",
                            "enum": ["upstream", "downstream"],
                            "description": "Traversal direction (default: upstream)",
                            "default": "upstream"
                        },
                        "depth": {
                            "type": "integer",
                            "description": "Maximum traversal depth (default: 3, max: 10)",
                            "default": 3
                        },
                        "include_tests": {
                            "type": "boolean",
                            "description": "Include test files in results (default: false)",
                            "default": false
                        },
                        "min_confidence": {
                            "type": "number",
                            "description": "Minimum edge confidence (0.0-1.0) to include"
                        },
                        "budget": {
                            "type": "integer",
                            "description": "Limit output to approximately N tokens"
                        },
                        "format": {
                            "type": "string",
                            "enum": ["json", "toon"],
                            "description": "Output format (default: json)",
                            "default": "json"
                        }
                    },
                    "required": ["symbol"]
                }),
            },
            Tool {
                name: "wonk_changes",
                description: "Detect changed symbols in working tree with optional blast radius and flow analysis.",
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "scope": {
                            "type": "string",
                            "enum": ["unstaged", "staged", "all", "compare"],
                            "description": "Change scope (default: unstaged)",
                            "default": "unstaged"
                        },
                        "base": {
                            "type": "string",
                            "description": "Base git ref for compare scope (required when scope=compare)"
                        },
                        "blast": {
                            "type": "boolean",
                            "description": "Include blast radius per changed symbol (default: false)",
                            "default": false
                        },
                        "flows": {
                            "type": "boolean",
                            "description": "Identify affected execution flows (default: false)",
                            "default": false
                        },
                        "min_confidence": {
                            "type": "number",
                            "description": "Minimum edge confidence (0.0-1.0) for blast/flow analysis"
                        },
                        "format": {
                            "type": "string",
                            "enum": ["json", "toon"],
                            "description": "Output format (default: json)",
                            "default": "json"
                        }
                    }
                }),
            },
            Tool {
                name: "wonk_context",
                description: "Get everything about a symbol in ONE call: definition, callers, callees, imports, and children. Replaces chaining wonk_sym + wonk_callers + wonk_callees + wonk_ref.",
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "name": {
                            "type": "string",
                            "description": "Symbol name to look up"
                        },
                        "file": {
                            "type": "string",
                            "description": "Restrict to symbols in this file"
                        },
                        "kind": {
                            "type": "string",
                            "description": "Filter by symbol kind (e.g. function, class)"
                        },
                        "min_confidence": {
                            "type": "number",
                            "description": "Minimum edge confidence (0.0-1.0) to include"
                        },
                        "format": {
                            "type": "string",
                            "enum": ["json", "toon"],
                            "description": "Output format (default: json)",
                            "default": "json"
                        }
                    },
                    "required": ["name"]
                }),
            },
            Tool {
                name: "wonk_ask",
                description: "Semantic search via embeddings. Requires Ollama. Use from/to for dependency scoping.",
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "query": {
                            "type": "string",
                            "description": "Natural language query to search for semantically similar code symbols"
                        },
                        "from": {
                            "type": "string",
                            "description": "Restrict results to symbols reachable from this file (dependency scoping)"
                        },
                        "to": {
                            "type": "string",
                            "description": "Restrict results to symbols that reach this file (reverse dependency scoping)"
                        },
                        "budget": {
                            "type": "integer",
                            "description": "Limit output to approximately N tokens"
                        },
                        "format": {
                            "type": "string",
                            "enum": ["json", "toon"],
                            "description": "Output format (default: json)",
                            "default": "json"
                        }
                    },
                    "required": ["query"]
                }),
            },
            Tool {
                name: "wonk_cluster",
                description: "Cluster symbols by semantic similarity using K-means on embeddings.",
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Scope clustering to symbols under this path prefix (relative to repo root)"
                        },
                        "top": {
                            "type": "integer",
                            "description": "Number of representative members to show per cluster (default: 5)",
                            "default": 5
                        },
                        "format": {
                            "type": "string",
                            "enum": ["json", "toon"],
                            "description": "Output format (default: json)",
                            "default": "json"
                        }
                    },
                    "required": ["path"]
                }),
            },
            Tool {
                name: "wonk_impact",
                description: "Analyze semantic impact of changed symbols. Use since for multi-file analysis.",
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "file": {
                            "type": "string",
                            "description": "File path (relative to repo root) to analyze for changed symbols"
                        },
                        "since": {
                            "type": "string",
                            "description": "Git ref (commit, tag, branch) — analyze all files changed since this ref"
                        },
                        "format": {
                            "type": "string",
                            "enum": ["json", "toon"],
                            "description": "Output format (default: json)",
                            "default": "json"
                        }
                    },
                    "required": ["file"]
                }),
            },
            Tool {
                name: "wonk_update",
                description: "Update the index and embeddings. Use force for a full rebuild.",
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "format": {
                            "type": "string",
                            "enum": ["json", "toon"],
                            "description": "Output format (default: json)",
                            "default": "json"
                        },
                        "force": {
                            "type": "boolean",
                            "description": "Force a full rebuild even if the index appears current",
                            "default": false
                        }
                    }
                }),
            },
            Tool {
                name: "wonk_delegate",
                description: "Ask a natural-language question about the codebase. Uses semantic search to gather relevant code context, then delegates to a local LLM (Ollama) for a grounded answer. Requires Ollama.",
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "question": {
                            "type": "string",
                            "description": "Natural language question about the codebase"
                        },
                        "scope": {
                            "type": "string",
                            "description": "Restrict context to symbols under this path prefix (e.g. 'src/auth/')"
                        },
                        "format": {
                            "type": "string",
                            "enum": ["json", "toon"],
                            "description": "Output format (default: json)",
                            "default": "json"
                        }
                    },
                    "required": ["question"]
                }),
            },
        ];

        // Inject optional `repo` parameter into all existing tools except wonk_init
        // and wonk_update (both always operate on the working directory repo).
        for tool in &mut tools {
            if tool.name == "wonk_init" || tool.name == "wonk_update" {
                continue;
            }
            if let Some(props) = tool.input_schema.get_mut("properties")
                && let Some(obj) = props.as_object_mut()
            {
                obj.insert("repo".to_string(), repo_prop.clone());
            }
        }

        // Add the wonk_repos tool (does not get repo param — it lists all repos).
        tools.push(Tool {
            name: "wonk_repos",
            description: "List all indexed repositories available for querying.",
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "format": {
                        "type": "string",
                        "enum": ["json", "toon"],
                        "description": "Output format (default: json)",
                        "default": "json"
                    }
                }
            }),
        });

        tools
    })
}

// ---------------------------------------------------------------------------
// Multi-repo registry
// ---------------------------------------------------------------------------

/// Metadata for a discovered indexed repository.
#[derive(Debug, Clone)]
struct RepoEntry {
    /// Absolute path to the repository root.
    repo_path: PathBuf,
    /// Absolute path to the index.db file.
    index_path: PathBuf,
    /// Short name (last component of repo_path).
    name: String,
    /// Languages detected during indexing.
    languages: Vec<String>,
    /// Unix timestamp when the index was created.
    created: u64,
}

/// Registry of all discovered indexed repositories.
struct RepoRegistry {
    entries: Vec<RepoEntry>,
    /// Lazy-opened connections keyed by index_path string.
    connections: HashMap<String, Connection>,
}

/// Result of resolving a repo reference.
#[derive(Debug)]
struct ResolvedRepo {
    index_path: PathBuf,
    repo_path: PathBuf,
}

impl RepoRegistry {
    fn new(entries: Vec<RepoEntry>) -> Self {
        Self {
            entries,
            connections: HashMap::new(),
        }
    }

    /// Resolve a repo name to a single `ResolvedRepo`.
    ///
    /// Matches by last path component of the repo root. Returns an error
    /// listing all matching paths if the name is ambiguous.
    fn resolve(&self, name: &str) -> Result<ResolvedRepo, String> {
        let matches: Vec<&RepoEntry> = self.entries.iter().filter(|e| e.name == name).collect();

        match matches.len() {
            0 => {
                let available: Vec<&str> = self.entries.iter().map(|e| e.name.as_str()).collect();
                Err(format!(
                    "unknown repo '{}'; available repos: {}",
                    name,
                    if available.is_empty() {
                        "(none)".to_string()
                    } else {
                        available.join(", ")
                    }
                ))
            }
            1 => {
                let entry = matches[0];
                Ok(ResolvedRepo {
                    index_path: entry.index_path.clone(),
                    repo_path: entry.repo_path.clone(),
                })
            }
            _ => {
                let paths: Vec<String> = matches
                    .iter()
                    .map(|e| e.repo_path.display().to_string())
                    .collect();
                Err(format!(
                    "ambiguous repo name '{}'; matches: {}",
                    name,
                    paths.join(", ")
                ))
            }
        }
    }

    /// Get or lazily open a connection for the given index path.
    fn get_or_open_connection(&mut self, index_path: &Path) -> Result<&Connection, String> {
        let key = index_path.to_string_lossy().into_owned();
        if !self.connections.contains_key(&key) {
            let conn = db::open_existing(index_path)
                .map_err(|e| format!("failed to open index at {}: {e}", index_path.display()))?;
            self.connections.insert(key.clone(), conn);
        }
        Ok(self.connections.get(&key).expect("just inserted"))
    }
}

/// Discover all indexed repositories under a repos directory.
///
/// Scans `repos_dir/*/index.db`, reads the adjacent `meta.json` for metadata,
/// and validates that the claimed repo path contains a `.git` or `.wonk` marker.
fn discover_repos(repos_dir: &Path) -> Vec<RepoEntry> {
    let mut entries = Vec::new();

    if repos_dir.is_dir()
        && let Ok(read_dir) = std::fs::read_dir(repos_dir)
    {
        for dir_entry in read_dir.flatten() {
            let index_dir = dir_entry.path();
            if !index_dir.is_dir() {
                continue;
            }
            let index_path = index_dir.join("index.db");
            if !index_path.exists() {
                continue;
            }
            if let Ok(meta) = db::read_meta(&index_path) {
                let repo_path = PathBuf::from(&meta.repo_path);
                // Validate the claimed repo path has a git or wonk marker.
                if !repo_path.join(".git").exists() && !repo_path.join(".wonk").exists() {
                    continue;
                }
                let name = repo_path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "unknown".to_string());
                entries.push(RepoEntry {
                    repo_path,
                    index_path,
                    name,
                    languages: meta.languages,
                    created: meta.created,
                });
            }
        }
    }

    entries
}

// ---------------------------------------------------------------------------
// MCP Server
// ---------------------------------------------------------------------------

struct McpServer {
    router: QueryRouter,
    registry: RepoRegistry,
}

impl McpServer {
    fn new(repo_root: PathBuf, registry: RepoRegistry) -> Self {
        let router = QueryRouter::new(Some(repo_root), false);
        Self { router, registry }
    }

    /// Resolve which repo connection and root to use for a tool call.
    ///
    /// If `args` contains a `"repo"` string, look it up in the registry and
    /// lazily open a connection. Otherwise, use the default (working directory)
    /// router's connection and repo root.
    ///
    /// Returns `Ok((conn_ref, repo_root))` or `Err(CallToolResult)` on failure.
    fn resolve_repo<'a>(
        &'a mut self,
        args: &Value,
    ) -> Result<(&'a Connection, PathBuf), CallToolResult> {
        if let Some(repo_name) = args.get("repo").and_then(|v| v.as_str()) {
            let resolved = self
                .registry
                .resolve(repo_name)
                .map_err(CallToolResult::error)?;
            let repo_path = resolved.repo_path.clone();
            let index_path = resolved.index_path.clone();
            let conn = self
                .registry
                .get_or_open_connection(&index_path)
                .map_err(CallToolResult::error)?;
            Ok((conn, repo_path))
        } else {
            match self.router.conn() {
                Some(c) => Ok((c, self.router.repo_root().to_path_buf())),
                None => Err(CallToolResult::error(
                    "no index available; run wonk_init first".into(),
                )),
            }
        }
    }

    fn handle_initialize(&self, _params: &Value) -> Value {
        serde_json::to_value(InitializeResult {
            protocol_version: PROTOCOL_VERSION,
            capabilities: ServerCapabilities {
                tools: ToolsCapability {},
            },
            server_info: ServerInfo {
                name: "wonk",
                version: env!("CARGO_PKG_VERSION"),
            },
            instructions: Some(
                "wonk provides structure-aware code analysis via a pre-built index. \
                 Prefer wonk tools over Glob/Read/Grep for code exploration — they return \
                 ranked, deduplicated results in fewer calls.\n\
                 - Architecture/module questions: wonk_summary with directory path + depth=1 \
                   (returns all files, symbols, and import edges in one call — do NOT call per-file)\n\
                 - Find a symbol definition: wonk_sym\n\
                 - Read symbol source code: wonk_show (batch: comma-separated names)\n\
                 - Find references/call sites: wonk_ref\n\
                 - Text search: wonk_search (keyword/regex, ranked, definitions first)\n\
                 - Semantic / natural-language search: wonk_ask (requires embeddings)",
            ),
        })
        .expect("serialize InitializeResult")
    }

    fn handle_tools_list(&self) -> Value {
        serde_json::json!({ "tools": tool_definitions() })
    }

    fn handle_tools_call(&mut self, params: &Value) -> Value {
        let call: CallToolParams = match serde_json::from_value(params.clone()) {
            Ok(p) => p,
            Err(_) => {
                return serde_json::to_value(CallToolResult::error(
                    "invalid tool call parameters".into(),
                ))
                .expect("serialize CallToolResult");
            }
        };

        let result = match call.name.as_str() {
            "wonk_search" => self.tool_search(call.arguments),
            "wonk_sym" => self.tool_sym(call.arguments),
            "wonk_ref" => self.tool_ref(call.arguments),
            "wonk_sig" => self.tool_sig(call.arguments),
            "wonk_deps" => self.tool_deps(call.arguments),
            "wonk_rdeps" => self.tool_rdeps(call.arguments),
            "wonk_status" => self.tool_status(call.arguments),
            "wonk_init" => self.tool_init(call.arguments),
            "wonk_show" => self.tool_show(call.arguments),
            "wonk_callers" => self.tool_callers(call.arguments),
            "wonk_callees" => self.tool_callees(call.arguments),
            "wonk_callpath" => self.tool_callpath(call.arguments),
            "wonk_summary" => self.tool_summary(call.arguments),
            "wonk_flows" => self.tool_flows(call.arguments),
            "wonk_blast" => self.tool_blast(call.arguments),
            "wonk_changes" => self.tool_changes(call.arguments),
            "wonk_context" => self.tool_context(call.arguments),
            "wonk_repos" => self.tool_repos(call.arguments),
            "wonk_ask" => self.tool_ask(call.arguments),
            "wonk_cluster" => self.tool_cluster(call.arguments),
            "wonk_impact" => self.tool_impact(call.arguments),
            "wonk_update" => self.tool_update(call.arguments),
            "wonk_delegate" => self.tool_delegate(call.arguments),
            _ => CallToolResult::error(format!("unknown tool: {}", call.name)),
        };

        serde_json::to_value(result).expect("serialize CallToolResult")
    }

    // -- Tool handlers -------------------------------------------------------

    fn tool_search(&mut self, args: Value) -> CallToolResult {
        let query = match require_str(&args, "query") {
            Ok(q) => q,
            Err(e) => return e,
        };
        let explicit_regex = args.get("regex").and_then(|v| v.as_bool()).unwrap_or(false);
        let regex = explicit_regex || search::looks_like_regex(&query);
        let case_insensitive = args
            .get("case_insensitive")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let paths: Vec<String> = args
            .get("paths")
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or_default();
        let budget_limit: Option<usize> = args
            .get("budget")
            .and_then(|v| v.as_u64())
            .map(|v| v as usize)
            .or(Some(4000));
        let format = extract_format(&args);

        let (ranker_conn, repo_root) = match self.resolve_repo(&args) {
            Ok((conn, root)) => (Some(conn), root),
            Err(e) => return e,
        };

        // Resolve user-supplied paths: accept relative paths and partial fragments.
        let mut resolved_paths: Vec<String> = Vec::new();
        for p in &paths {
            let as_path = Path::new(p);
            if as_path.is_absolute() {
                return CallToolResult::error(format!("absolute paths are not allowed: {p}"));
            }
            let joined = repo_root.join(as_path);
            if joined.exists() {
                resolved_paths.push(joined.to_string_lossy().into_owned());
            } else {
                // Fuzzy: try as a substring match against repo root children.
                let mut found = false;
                if let Ok(entries) = std::fs::read_dir(&repo_root) {
                    for entry in entries.flatten() {
                        let name = entry.file_name().to_string_lossy().into_owned();
                        if name.contains(p.as_str()) && entry.path().is_dir() {
                            resolved_paths.push(entry.path().to_string_lossy().into_owned());
                            found = true;
                        }
                    }
                }
                if !found {
                    // Fall through: use repo root join as-is (grep will simply find nothing).
                    resolved_paths.push(joined.to_string_lossy().into_owned());
                }
            }
        }

        // Default to repo root when no paths specified.
        if resolved_paths.is_empty() {
            resolved_paths.push(repo_root.to_string_lossy().into_owned());
        }

        let results = match search::text_search(&query, regex, case_insensitive, &resolved_paths) {
            Ok(r) => r,
            Err(e) => return CallToolResult::error(format!("search failed: {e}")),
        };

        let groups = ranker::rank_and_dedup(&results, ranker_conn, &query);

        let mut budget = budget_limit.map(TokenBudget::new);
        let mut outputs: Vec<SearchOutput> = Vec::new();
        let mut truncated = 0usize;

        for (_category, items) in &groups {
            for item in items {
                let mut out = SearchOutput::from_search_result(
                    &item.result.file,
                    item.result.line,
                    item.result.col,
                    &item.result.content,
                );
                out.annotation = item.annotation.clone();

                if let Some(ref mut b) = budget {
                    let estimate = (out.file.len() + out.content.len() + 20) / 4;
                    if b.remaining() < estimate {
                        truncated += 1;
                        continue;
                    }
                    let serialized = serde_json::to_string(&out).unwrap_or_default();
                    if !b.try_consume(&serialized) {
                        truncated += 1;
                        continue;
                    }
                }
                outputs.push(out);
            }
        }

        if truncated > 0 {
            let shown = outputs.len();
            let total = shown + truncated;
            let wrapper = serde_json::json!({
                "results": outputs,
                "truncated": truncated,
                "hint": format!("Showing {shown} of {total} matches. Results are sorted by relevance."),
            });
            format_result(&wrapper, format)
        } else {
            format_result(&outputs, format)
        }
    }

    fn tool_sym(&mut self, args: Value) -> CallToolResult {
        let name = match require_str(&args, "name") {
            Ok(n) => n,
            Err(e) => return e,
        };
        let kind = args.get("kind").and_then(|v| v.as_str());
        let file = args.get("file").and_then(|v| v.as_str());
        let exact = args.get("exact").and_then(|v| v.as_bool()).unwrap_or(false);
        let format = extract_format(&args);

        let (conn, _) = match self.resolve_repo(&args) {
            Ok(r) => r,
            Err(e) => return e,
        };
        match crate::router::query_symbols_db_with_file(conn, &name, kind, file, exact) {
            Ok(r) if r.is_empty() => {
                let hints = empty_show_hints(conn, &name, None, kind);
                let wrapper = serde_json::json!({
                    "results": Vec::<String>::new(),
                    "hints": hints,
                });
                format_result(&wrapper, format)
            }
            Ok(mut r) => {
                // Deprioritize .d.ts files — push them to the end so
                // actual source definitions appear first within budget.
                r.sort_by(|a, b| {
                    let a_dts = a.file.ends_with(".d.ts");
                    let b_dts = b.file.ends_with(".d.ts");
                    a_dts.cmp(&b_dts)
                });
                let outputs: Vec<SymbolOutput> = r.iter().map(symbol_to_output).collect();
                format_result(&outputs, format)
            }
            Err(e) => CallToolResult::error(format!("symbol query failed: {e}")),
        }
    }

    fn tool_ref(&mut self, args: Value) -> CallToolResult {
        let name = match require_str(&args, "name") {
            Ok(n) => n,
            Err(e) => return e,
        };
        let budget_limit: Option<usize> = args
            .get("budget")
            .and_then(|v| v.as_u64())
            .map(|v| v as usize)
            .or(Some(2000));
        let format = extract_format(&args);
        let output_mode = args
            .get("output")
            .and_then(|v| v.as_str())
            .unwrap_or("full");

        let (conn, repo_root) = match self.resolve_repo(&args) {
            Ok(r) => r,
            Err(e) => return e,
        };
        let results = match crate::router::query_references_db(conn, &name) {
            Ok(r) => r,
            Err(e) => return CallToolResult::error(format!("reference query failed: {e}")),
        };

        // Also query subclasses/implementors from type_edges.
        let subclass_results = crate::router::query_subclasses_db(conn, &name).unwrap_or_default();

        // Files-only mode: return just unique file paths.
        if output_mode == "files" {
            let mut files: Vec<String> = results.iter().map(|r| r.file.clone()).collect();
            files.extend(subclass_results.iter().map(|s| s.file.clone()));
            files.sort();
            files.dedup();
            return format_result(&files, format);
        }

        let mut outputs: Vec<RefOutput> = Vec::new();

        // Subclasses first.
        for sym in &subclass_results {
            outputs.push(RefOutput {
                name: sym.name.clone(),
                kind: "subclass".to_string(),
                file: sym.file.clone(),
                line: sym.line,
                col: sym.col,
                context: sym.signature.clone(),
                caller_name: None,
                confidence: 1.0,
            });
        }

        // Then regular references.
        for r in &results {
            let context = enrich_context(&repo_root, &r.file, r.line, &r.context);
            outputs.push(RefOutput {
                name: r.name.clone(),
                kind: r.kind.to_string(),
                file: r.file.clone(),
                line: r.line,
                col: r.col,
                context,
                caller_name: r.caller_name.clone(),
                confidence: r.confidence,
            });
        }

        collect_with_budget(outputs, budget_limit, format)
    }

    fn tool_sig(&mut self, args: Value) -> CallToolResult {
        let name = match require_str(&args, "name") {
            Ok(n) => n,
            Err(e) => return e,
        };
        let format = extract_format(&args);

        let (conn, _) = match self.resolve_repo(&args) {
            Ok(r) => r,
            Err(e) => return e,
        };
        let results = match crate::router::query_signatures_db(conn, &name) {
            Ok(r) => r,
            Err(e) => return CallToolResult::error(format!("signature query failed: {e}")),
        };

        let outputs: Vec<SignatureOutput> = results
            .iter()
            .map(|sym| SignatureOutput {
                name: sym.name.clone(),
                file: sym.file.clone(),
                line: sym.line,
                signature: sym.signature.clone(),
                language: sym.language.clone(),
            })
            .collect();

        format_result(&outputs, format)
    }

    fn tool_deps(&mut self, args: Value) -> CallToolResult {
        let file = match require_str(&args, "file") {
            Ok(f) => f,
            Err(e) => return e,
        };
        let format = extract_format(&args);

        let (conn, repo_root) = match self.resolve_repo(&args) {
            Ok(r) => r,
            Err(e) => return e,
        };
        if validate_path(Path::new(&file), &repo_root).is_err() {
            return CallToolResult::error("path is outside the repository".into());
        }
        let results = match crate::router::query_deps_db(conn, &file) {
            Ok(r) => r,
            Err(e) => return CallToolResult::error(format!("dependency query failed: {e}")),
        };

        let outputs: Vec<DepOutput> = results
            .iter()
            .map(|dep| DepOutput {
                file: file.clone(),
                depends_on: dep.clone(),
            })
            .collect();

        format_result(&outputs, format)
    }

    fn tool_rdeps(&mut self, args: Value) -> CallToolResult {
        let file = match require_str(&args, "file") {
            Ok(f) => f,
            Err(e) => return e,
        };
        let format = extract_format(&args);

        let (conn, repo_root) = match self.resolve_repo(&args) {
            Ok(r) => r,
            Err(e) => return e,
        };
        if validate_path(Path::new(&file), &repo_root).is_err() {
            return CallToolResult::error("path is outside the repository".into());
        }
        let results = match crate::router::query_rdeps_db(conn, &file) {
            Ok(r) => r,
            Err(e) => {
                return CallToolResult::error(format!("reverse dependency query failed: {e}"));
            }
        };

        let outputs: Vec<DepOutput> = results
            .iter()
            .map(|source| DepOutput {
                file: source.clone(),
                depends_on: file.clone(),
            })
            .collect();

        format_result(&outputs, format)
    }

    fn tool_status(&mut self, args: Value) -> CallToolResult {
        let format = extract_format(&args);
        // status works even without a connection (shows "not indexed").
        let conn = if let Some(repo_name) = args.get("repo").and_then(|v| v.as_str()) {
            let resolved = match self.registry.resolve(repo_name) {
                Ok(r) => r,
                Err(e) => return CallToolResult::error(e),
            };
            match self.registry.get_or_open_connection(&resolved.index_path) {
                Ok(c) => Some(c as &Connection),
                Err(e) => return CallToolResult::error(e),
            }
        } else {
            self.router.conn()
        };
        let info = crate::router::query_status_info(conn);
        let status = serde_json::to_value(&info).unwrap_or_default();
        format_result(&status, format)
    }

    fn tool_init(&mut self, args: Value) -> CallToolResult {
        let local = args.get("local").and_then(|v| v.as_bool()).unwrap_or(false);
        let format = extract_format(&args);

        let stats = match pipeline::build_index_with_progress(
            self.router.repo_root(),
            local,
            &Progress::silent(),
        ) {
            Ok(s) => s,
            Err(e) => return CallToolResult::error(format!("index build failed: {e}")),
        };

        // Build embeddings after structural index.
        let index_path = match db::index_path_for(self.router.repo_root(), local) {
            Ok(p) => p,
            Err(_) => {
                let result = serde_json::json!({
                    "file_count": stats.file_count,
                    "symbol_count": stats.symbol_count,
                    "reference_count": stats.ref_count,
                    "elapsed_ms": stats.elapsed.as_millis(),
                    "embedding_count": 0,
                    "embedding_skipped": true
                });
                return format_result(&result, format);
            }
        };

        let emb_stats = db::open(&index_path)
            .ok()
            .and_then(|conn| {
                let client = crate::embedding::OllamaClient::new();
                pipeline::build_embeddings(
                    &conn,
                    self.router.repo_root(),
                    &client,
                    crate::progress::ProgressMode::Silent,
                )
                .ok()
            })
            .unwrap_or(pipeline::EmbeddingBuildStats {
                embedded_count: 0,
                total_symbols: 0,
                skipped: true,
                elapsed: std::time::Duration::ZERO,
            });

        let result = serde_json::json!({
            "file_count": stats.file_count,
            "symbol_count": stats.symbol_count,
            "reference_count": stats.ref_count,
            "elapsed_ms": stats.elapsed.as_millis(),
            "embedding_count": emb_stats.embedded_count,
            "embedding_skipped": emb_stats.skipped
        });
        format_result(&result, format)
    }

    fn tool_show(&mut self, args: Value) -> CallToolResult {
        let raw_name = match require_str(&args, "name") {
            Ok(n) => n,
            Err(e) => return e,
        };
        let kind = args.get("kind").and_then(|v| v.as_str()).map(String::from);
        let explicit_file = args.get("file").and_then(|v| v.as_str()).map(String::from);
        let exact = args.get("exact").and_then(|v| v.as_bool()).unwrap_or(false);

        // Support qualified paths: `tokio::spawn` → name="spawn", file hint="tokio".
        let (name, file) = if raw_name.contains("::") && explicit_file.is_none() {
            if let Some(pos) = raw_name.rfind("::") {
                let prefix = &raw_name[..pos];
                let bare = &raw_name[pos + 2..];
                if bare.is_empty() {
                    (raw_name.clone(), explicit_file)
                } else {
                    (bare.to_string(), Some(prefix.replace("::", "/")))
                }
            } else {
                (raw_name, explicit_file)
            }
        } else {
            (raw_name, explicit_file)
        };
        let shallow = args
            .get("shallow")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let budget_limit: Option<usize> = args
            .get("budget")
            .and_then(|v| v.as_u64())
            .map(|v| v as usize)
            .or(Some(4000));
        let format = extract_format(&args);

        let (conn, repo_root) = match self.resolve_repo(&args) {
            Ok(r) => r,
            Err(e) => return e,
        };

        let options = crate::show::ShowOptions {
            file: file.clone(),
            kind: kind.clone(),
            exact,
            suppress: true,
            shallow,
        };

        // Support comma-separated names for batch lookup (e.g. "main,parse,validate").
        let names: Vec<&str> = name
            .split(',')
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .collect();

        let mut all_results = Vec::new();
        for n in &names {
            match crate::show::show_symbol(conn, n, &repo_root, &options) {
                Ok(r) => all_results.extend(r),
                Err(e) => return CallToolResult::error(format!("show query failed: {e}")),
            }
        }

        // Empty result diagnostics (Fix 11).
        if all_results.is_empty() {
            let hints = empty_show_hints(conn, &name, file.as_deref(), kind.as_deref());
            let wrapper = serde_json::json!({
                "results": Vec::<String>::new(),
                "hints": hints,
            });
            return format_result(&wrapper, format);
        }

        let mut budget = budget_limit.map(TokenBudget::new);
        let mut outputs: Vec<ShowOutput> = Vec::new();
        let mut truncated = 0usize;

        for sr in &all_results {
            let out = ShowOutput::from(sr);

            if let Some(ref mut b) = budget {
                let serialized = serde_json::to_string(&out).unwrap_or_default();
                if !b.try_consume(&serialized) {
                    // Auto-fallback: retry container types in shallow mode
                    if !shallow && sr.kind.is_container() {
                        let shallow_opts = crate::show::ShowOptions {
                            file: Some(sr.file.clone()),
                            kind: Some(sr.kind.to_string()),
                            exact: true,
                            suppress: true,
                            shallow: true,
                        };
                        if let Ok(shallow_results) =
                            crate::show::show_symbol(conn, &sr.name, &repo_root, &shallow_opts)
                            && let Some(shallow_sr) = shallow_results
                                .iter()
                                .find(|s| s.file == sr.file && s.line == sr.line)
                        {
                            let mut shallow_out = ShowOutput::from(shallow_sr);
                            shallow_out.auto_shallow = Some(true);
                            let shallow_ser =
                                serde_json::to_string(&shallow_out).unwrap_or_default();
                            if b.try_consume(&shallow_ser) {
                                outputs.push(shallow_out);
                                continue;
                            }
                        }
                    }
                    truncated += 1;
                    continue;
                }
            }
            outputs.push(out);
        }

        if truncated > 0 {
            let shown = outputs.len();
            let total = shown + truncated;
            let wrapper = serde_json::json!({
                "results": outputs,
                "truncated": truncated,
                "hint": format!("Showing {shown} of {total} matching symbols. Results are sorted by relevance."),
            });
            format_result(&wrapper, format)
        } else {
            format_result(&outputs, format)
        }
    }

    /// Shared setup for callgraph tools: extract depth + budget + format,
    /// open connection, and verify caller_id data exists.
    fn callgraph_setup(
        &mut self,
        args: &Value,
    ) -> Result<(&Connection, usize, Option<usize>, OutputFormat), CallToolResult> {
        let depth_raw = args.get("depth").and_then(|v| v.as_u64()).unwrap_or(1) as usize;
        let (depth, _) = crate::callgraph::clamp_depth(depth_raw);
        let budget_limit: Option<usize> = args
            .get("budget")
            .and_then(|v| v.as_u64())
            .map(|v| v as usize)
            .or(Some(2000));
        let format = extract_format(args);

        let (conn, _) = self.resolve_repo(args)?;

        if !crate::callgraph::has_caller_id_data(conn) {
            return Err(CallToolResult::error(
                "index lacks call graph data; run wonk_init to re-index".into(),
            ));
        }

        Ok((conn, depth, budget_limit, format))
    }

    fn tool_callers(&mut self, args: Value) -> CallToolResult {
        let name = match require_str(&args, "name") {
            Ok(n) => n,
            Err(e) => return e,
        };
        let (conn, depth, budget_limit, format) = match self.callgraph_setup(&args) {
            Ok(setup) => setup,
            Err(e) => return e,
        };

        let min_confidence: Option<f64> = args
            .get("min_confidence")
            .and_then(|v| v.as_f64())
            .map(clamp_confidence);

        let results = match crate::callgraph::callers(conn, &name, depth, min_confidence) {
            Ok(r) => r,
            Err(e) => return CallToolResult::error(format!("callers query failed: {e}")),
        };

        let outputs: Vec<CallerOutput> = results
            .iter()
            .map(|cr| CallerOutput {
                caller_name: cr.caller_name.clone(),
                caller_kind: cr.caller_kind.to_string(),
                file: cr.file.clone(),
                line: cr.line,
                signature: cr.signature.clone(),
                depth: cr.depth,
                target_file: cr.target_file.clone(),
                confidence: cr.confidence,
            })
            .collect();

        collect_with_budget(outputs, budget_limit, format)
    }

    fn tool_callees(&mut self, args: Value) -> CallToolResult {
        let name = match require_str(&args, "name") {
            Ok(n) => n,
            Err(e) => return e,
        };
        let (conn, depth, budget_limit, format) = match self.callgraph_setup(&args) {
            Ok(setup) => setup,
            Err(e) => return e,
        };

        let min_confidence: Option<f64> = args
            .get("min_confidence")
            .and_then(|v| v.as_f64())
            .map(clamp_confidence);

        let results = match crate::callgraph::callees(conn, &name, depth, min_confidence) {
            Ok(r) => r,
            Err(e) => return CallToolResult::error(format!("callees query failed: {e}")),
        };

        let outputs: Vec<CalleeOutput> = results
            .iter()
            .map(|cr| CalleeOutput {
                callee_name: cr.callee_name.clone(),
                file: cr.file.clone(),
                line: cr.line,
                context: cr.context.clone(),
                depth: cr.depth,
                source_file: cr.source_file.clone(),
                confidence: cr.confidence,
            })
            .collect();

        collect_with_budget(outputs, budget_limit, format)
    }

    fn tool_callpath(&mut self, args: Value) -> CallToolResult {
        let from = match require_str(&args, "from") {
            Ok(n) => n,
            Err(e) => return e,
        };
        let to = match require_str(&args, "to") {
            Ok(n) => n,
            Err(e) => return e,
        };
        let format = extract_format(&args);

        let (conn, _) = match self.resolve_repo(&args) {
            Ok(r) => r,
            Err(e) => return e,
        };

        if !crate::callgraph::has_caller_id_data(conn) {
            return CallToolResult::error(
                "index lacks call graph data; run wonk_init to re-index".into(),
            );
        }

        let min_confidence: Option<f64> = args
            .get("min_confidence")
            .and_then(|v| v.as_f64())
            .map(clamp_confidence);

        match crate::callgraph::callpath(conn, &from, &to, min_confidence) {
            Ok(Some(hops)) => {
                let outputs: Vec<CallPathHopOutput> = hops
                    .iter()
                    .map(|h| CallPathHopOutput {
                        symbol_name: h.symbol_name.clone(),
                        symbol_kind: h.symbol_kind.to_string(),
                        file: h.file.clone(),
                        line: h.line,
                    })
                    .collect();
                format_result(&outputs, format)
            }
            Ok(None) => format_result(&Vec::<CallPathHopOutput>::new(), format),
            Err(e) => CallToolResult::error(format!("callpath query failed: {e}")),
        }
    }

    fn tool_summary(&mut self, args: Value) -> CallToolResult {
        let path = match require_str(&args, "path") {
            Ok(p) => p,
            Err(e) => return e,
        };

        let (conn, repo_root) = match self.resolve_repo(&args) {
            Ok(r) => r,
            Err(e) => return e,
        };

        // Validate path stays within repo boundary.
        if let Err(e) = validate_path(Path::new(&path), &repo_root) {
            return e;
        }

        // MCP always uses outline — rich is only available via CLI.
        let detail = crate::types::DetailLevel::Outline;

        let recursive = args
            .get("recursive")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let depth_raw = args.get("depth").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
        let depth = if recursive { None } else { Some(depth_raw) };

        let format = extract_format(&args);

        if let Err(e) = crate::db::ensure_summaries_table(conn) {
            return CallToolResult::error(format!("failed to create summaries table: {e}"));
        }

        let options = crate::summary::SummaryOptions {
            detail,
            depth,
            suppress: true,
        };

        let result = match crate::summary::summarize_path(conn, &path, &options) {
            Ok(r) => r,
            Err(e) => return CallToolResult::error(format!("summary query failed: {e}")),
        };

        let out = SummaryOutput::from_result(&result);
        format_result(&out, format)
    }

    fn tool_blast(&mut self, args: Value) -> CallToolResult {
        let symbol = match require_str(&args, "symbol") {
            Ok(s) => s,
            Err(e) => return e,
        };

        let (conn, _) = match self.resolve_repo(&args) {
            Ok(r) => r,
            Err(e) => return e,
        };

        if !crate::callgraph::has_caller_id_data(conn) {
            return CallToolResult::error(
                "index lacks call graph data; run wonk_init to re-index".into(),
            );
        }

        let format = extract_format(&args);

        let depth_raw = args
            .get("depth")
            .and_then(|v| v.as_u64())
            .unwrap_or(crate::blast::DEFAULT_DEPTH as u64) as usize;
        let (depth, _clamped) = crate::blast::clamp_depth(depth_raw);

        let direction_str = args
            .get("direction")
            .and_then(|v| v.as_str())
            .unwrap_or("upstream");
        let direction = match direction_str.parse::<crate::types::BlastDirection>() {
            Ok(d) => d,
            Err(e) => return CallToolResult::error(e),
        };

        let include_tests = args
            .get("include_tests")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let min_confidence: Option<f64> = args.get("min_confidence").and_then(|v| v.as_f64());

        let options = crate::blast::BlastOptions {
            depth,
            direction,
            include_tests,
            min_confidence,
        };

        match crate::blast::analyze_blast(conn, &symbol, &options) {
            Ok(ref analysis) => {
                let out = crate::output::BlastOutput::from(analysis);
                format_result(&out, format)
            }
            Err(e) => CallToolResult::error(format!("blast analysis failed: {e}")),
        }
    }

    fn tool_flows(&mut self, args: Value) -> CallToolResult {
        let (conn, repo_root) = match self.resolve_repo(&args) {
            Ok(r) => r,
            Err(e) => return e,
        };

        if !crate::callgraph::has_caller_id_data(conn) {
            return CallToolResult::error(
                "index lacks call graph data; run wonk_init to re-index".into(),
            );
        }

        let format = extract_format(&args);
        let budget_limit: Option<usize> = args
            .get("budget")
            .and_then(|v| v.as_u64())
            .map(|v| v as usize);

        let depth_raw = args
            .get("depth")
            .and_then(|v| v.as_u64())
            .unwrap_or(crate::flows::DEFAULT_DEPTH as u64) as usize;
        let (depth, _clamped) = crate::flows::clamp_depth(depth_raw);

        let branching = args
            .get("branching")
            .and_then(|v| v.as_u64())
            .unwrap_or(crate::flows::DEFAULT_BRANCHING as u64) as usize;

        let min_confidence: Option<f64> = args.get("min_confidence").and_then(|v| v.as_f64());

        let from_file = args.get("from").and_then(|v| v.as_str()).map(String::from);
        if let Some(ref f) = from_file
            && validate_path(std::path::Path::new(f), &repo_root).is_err()
        {
            return CallToolResult::error(format!("path outside repository: {f}"));
        }

        let options = crate::flows::FlowOptions {
            depth,
            branching,
            min_confidence,
            from_file,
        };

        let entry = args.get("entry").and_then(|v| v.as_str());

        if let Some(entry_name) = entry {
            // Trace mode.
            match crate::flows::trace_flow(conn, entry_name, &options) {
                Ok(Some(ref flow)) => {
                    let out = crate::output::FlowOutput::from(flow);
                    format_result(&out, format)
                }
                Ok(None) => format_result(&serde_json::json!({"message": "no flow found"}), format),
                Err(e) => CallToolResult::error(format!("flows query failed: {e}")),
            }
        } else {
            // List mode: detect entry points.
            match crate::flows::detect_entry_points(conn, &options) {
                Ok(entries) => {
                    let outputs: Vec<crate::output::FlowStepOutput> = entries
                        .iter()
                        .map(crate::output::FlowStepOutput::from)
                        .collect();
                    collect_with_budget(outputs, budget_limit, format)
                }
                Err(e) => CallToolResult::error(format!("flows query failed: {e}")),
            }
        }
    }

    fn tool_changes(&mut self, args: Value) -> CallToolResult {
        let (conn, repo_root) = match self.resolve_repo(&args) {
            Ok(r) => r,
            Err(e) => return e,
        };

        let format = extract_format(&args);

        // Parse scope.
        let scope_str = args
            .get("scope")
            .and_then(|v| v.as_str())
            .unwrap_or("unstaged");
        let scope = if scope_str == "compare" {
            let base = match args.get("base").and_then(|v| v.as_str()) {
                Some(b) => b.to_string(),
                None => {
                    return CallToolResult::error("'base' is required when scope=compare".into());
                }
            };
            crate::types::ChangeScope::Compare(base)
        } else {
            match scope_str.parse::<crate::types::ChangeScope>() {
                Ok(s) => s,
                Err(e) => return CallToolResult::error(e),
            }
        };

        let blast = args.get("blast").and_then(|v| v.as_bool()).unwrap_or(false);
        let flows = args.get("flows").and_then(|v| v.as_bool()).unwrap_or(false);
        let min_confidence = args
            .get("min_confidence")
            .and_then(|v| v.as_f64())
            .map(clamp_confidence);

        // Detect changes.
        let analysis = match crate::impact::detect_changes(conn, &scope, &repo_root) {
            Ok(a) => a,
            Err(e) => return CallToolResult::error(format!("change detection failed: {e}")),
        };

        // Build output using shared helper.
        let changes_out = match crate::router::build_changes_output(
            conn,
            &analysis,
            &scope,
            &crate::router::ChangesChainOptions {
                blast,
                flows,
                min_confidence,
            },
            |_| {},
        ) {
            Ok(o) => o,
            Err(e) => return CallToolResult::error(format!("changes analysis failed: {e}")),
        };

        format_result(&changes_out, format)
    }

    fn tool_context(&mut self, args: Value) -> CallToolResult {
        let name = match require_str(&args, "name") {
            Ok(n) => n,
            Err(e) => return e,
        };

        let (conn, _) = match self.resolve_repo(&args) {
            Ok(r) => r,
            Err(e) => return e,
        };

        if !crate::callgraph::has_caller_id_data(conn) {
            return CallToolResult::error(
                "index lacks call graph data; run wonk_init to re-index".into(),
            );
        }

        let format = extract_format(&args);

        let file = args.get("file").and_then(|v| v.as_str()).map(String::from);
        let kind = args.get("kind").and_then(|v| v.as_str()).map(String::from);
        let min_confidence = args.get("min_confidence").and_then(|v| v.as_f64());

        let options = crate::context::ContextOptions {
            file,
            kind,
            min_confidence,
        };

        match crate::context::symbol_context(conn, &name, &options) {
            Ok(contexts) => {
                let outputs: Vec<crate::output::SymbolContextOutput> = contexts
                    .iter()
                    .map(crate::output::SymbolContextOutput::from)
                    .collect();
                format_result(&outputs, format)
            }
            Err(e) => CallToolResult::error(format!("context query failed: {e}")),
        }
    }

    fn tool_repos(&mut self, args: Value) -> CallToolResult {
        let format = extract_format(&args);

        #[derive(serde::Serialize)]
        struct RepoInfo {
            name: String,
            path: String,
            file_count: u64,
            symbol_count: u64,
            last_indexed: u64,
            languages: Vec<String>,
        }

        // Collect entry metadata first to avoid borrow conflict with get_or_open_connection.
        let entry_data: Vec<(PathBuf, String, String, u64, Vec<String>)> = self
            .registry
            .entries
            .iter()
            .map(|e| {
                (
                    e.index_path.clone(),
                    e.name.clone(),
                    e.repo_path.display().to_string(),
                    e.created,
                    e.languages.clone(),
                )
            })
            .collect();

        let mut repos: Vec<RepoInfo> = Vec::new();

        for (index_path, name, path, created, languages) in &entry_data {
            let (file_count, symbol_count) = match self.registry.get_or_open_connection(index_path)
            {
                Ok(conn) => {
                    let fc: i64 = conn
                        .query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0))
                        .unwrap_or(0);
                    let sc: i64 = conn
                        .query_row("SELECT COUNT(*) FROM symbols", [], |r| r.get(0))
                        .unwrap_or(0);
                    (fc as u64, sc as u64)
                }
                Err(_) => (0, 0),
            };

            repos.push(RepoInfo {
                name: name.clone(),
                path: path.clone(),
                file_count,
                symbol_count,
                last_indexed: *created,
                languages: languages.clone(),
            });
        }

        format_result(&repos, format)
    }

    fn tool_ask(&mut self, args: Value) -> CallToolResult {
        let query = match require_str(&args, "query") {
            Ok(q) => q,
            Err(e) => return e,
        };
        let budget_limit: Option<usize> = args
            .get("budget")
            .and_then(|v| v.as_u64())
            .map(|v| v as usize);
        let format = extract_format(&args);

        let (conn, repo_root) = match self.resolve_repo(&args) {
            Ok(r) => r,
            Err(e) => return e,
        };

        let from = args.get("from").and_then(|v| v.as_str());
        let to = args.get("to").and_then(|v| v.as_str());

        // Compute dependency-scoped file set for --from/--to filtering.
        let reachable_files = match crate::semantic::compute_reachable_files(conn, from, to) {
            Ok(r) => r,
            Err(e) => {
                return CallToolResult::error(format!("reachability computation failed: {e}"));
            }
        };

        // Load embeddings — scoped to reachable files when from/to is specified.
        let embeddings = match &reachable_files {
            Some(files) => match crate::embedding::load_embeddings_for_files(conn, files) {
                Ok(e) => e,
                Err(e) => return CallToolResult::error(format!("failed to load embeddings: {e}")),
            },
            None => match crate::embedding::load_all_embeddings(conn) {
                Ok(e) => e,
                Err(e) => return CallToolResult::error(format!("failed to load embeddings: {e}")),
            },
        };

        if embeddings.is_empty() {
            return CallToolResult::error(
                "no embeddings available; run wonk_init with Ollama running to build embeddings"
                    .into(),
            );
        }

        let client = crate::embedding::OllamaClient::new();
        let mut query_vec = match client.embed_single(&query) {
            Ok(v) => v,
            Err(e) => return CallToolResult::error(format!("embedding query failed: {e}")),
        };
        crate::embedding::normalize(&mut query_vec);

        let scored = crate::semantic::semantic_search(&query_vec, &embeddings, 50);
        let semantic_results = match crate::semantic::resolve_results(conn, &scored) {
            Ok(r) => r,
            Err(e) => return CallToolResult::error(format!("result resolution failed: {e}")),
        };

        // Run structural search with same query for RRF blending.
        // Keyword queries like "handleError" get both structural + semantic fused;
        // NL queries like "error handling in route handlers" gracefully degrade to
        // pure semantic (ripgrep returns nothing for natural language).
        let repo_path = repo_root.to_string_lossy().into_owned();
        let structural_results =
            search::text_search(&query, false, true, &[repo_path]).unwrap_or_default();

        if !structural_results.is_empty() {
            let fused = ranker::fuse_rrf(&structural_results, &semantic_results, 60.0);

            let mut budget = budget_limit.map(TokenBudget::new);
            let mut outputs: Vec<SearchOutput> = Vec::new();
            let mut truncated = 0usize;
            for item in &fused {
                let mut out = SearchOutput::from_search_result(
                    Path::new(&item.file),
                    item.line,
                    item.col,
                    &item.content,
                );
                out.annotation = item.annotation.clone();

                if let Some(ref mut b) = budget {
                    let estimate = (out.file.len() + out.content.len() + 20) / 4;
                    if b.remaining() < estimate {
                        truncated += 1;
                        continue;
                    }
                    let serialized = serde_json::to_string(&out).unwrap_or_default();
                    if !b.try_consume(&serialized) {
                        truncated += 1;
                        continue;
                    }
                }
                outputs.push(out);
            }
            if truncated > 0 {
                let shown = outputs.len();
                let total = shown + truncated;
                let wrapper = serde_json::json!({
                    "results": outputs,
                    "truncated": truncated,
                    "hint": format!("Showing {shown} of {total} matches. Results are sorted by relevance."),
                });
                return format_result(&wrapper, format);
            }
            return format_result(&outputs, format);
        }

        // Pure semantic results (NL queries won't match ripgrep).
        let outputs: Vec<crate::output::SemanticOutput> = semantic_results
            .iter()
            .map(|sr| crate::output::SemanticOutput {
                file: sr.file.clone(),
                line: sr.line,
                symbol_name: sr.symbol_name.clone(),
                symbol_kind: sr.symbol_kind.to_string(),
                similarity_score: sr.similarity_score,
                symbol_id: sr.symbol_id,
            })
            .collect();

        collect_with_budget(outputs, budget_limit, format)
    }

    fn tool_cluster(&mut self, args: Value) -> CallToolResult {
        let path = match require_str(&args, "path") {
            Ok(p) => p,
            Err(e) => return e,
        };
        let top = args.get("top").and_then(|v| v.as_u64()).unwrap_or(5) as usize;
        let format = extract_format(&args);

        let (conn, repo_root) = match self.resolve_repo(&args) {
            Ok(r) => r,
            Err(e) => return e,
        };

        if let Err(e) = validate_path(Path::new(&path), &repo_root) {
            return e;
        }

        // Normalize path: strip leading "./", normalize "." to empty.
        let prefix = path.strip_prefix("./").unwrap_or(&path);
        let prefix = if prefix == "." { "" } else { prefix };

        let embeddings = match crate::embedding::load_embeddings_for_path_prefix(conn, prefix) {
            Ok(e) => e,
            Err(e) => return CallToolResult::error(format!("failed to load embeddings: {e}")),
        };

        if embeddings.is_empty() {
            return CallToolResult::error(
                "no embeddings found for this path; run wonk_init with Ollama running to build embeddings".into(),
            );
        }

        let mut clusters =
            crate::cluster::cluster_embeddings(&embeddings, crate::cluster::ABSOLUTE_MAX_K);
        if let Err(e) = crate::cluster::resolve_cluster_members(conn, &mut clusters) {
            return CallToolResult::error(format!("cluster resolution failed: {e}"));
        }

        let outputs: Vec<crate::output::ClusterOutput> = clusters
            .iter()
            .map(|cluster| crate::output::ClusterOutput {
                cluster_id: cluster.cluster_id,
                total_members: cluster.members.len(),
                representatives: cluster
                    .members
                    .iter()
                    .take(top)
                    .map(|m| crate::output::ClusterMemberOutput {
                        file: m.file.clone(),
                        line: m.line,
                        symbol_name: m.symbol_name.clone(),
                        symbol_kind: m.symbol_kind.to_string(),
                        distance_to_centroid: m.distance_to_centroid,
                    })
                    .collect(),
            })
            .collect();

        format_result(&outputs, format)
    }

    fn tool_impact(&mut self, args: Value) -> CallToolResult {
        let file = match require_str(&args, "file") {
            Ok(f) => f,
            Err(e) => return e,
        };
        let since = args.get("since").and_then(|v| v.as_str()).map(String::from);
        let format = extract_format(&args);

        let (conn, repo_root) = match self.resolve_repo(&args) {
            Ok(r) => r,
            Err(e) => return e,
        };

        // Determine files to analyze.
        let files: Vec<String> = if let Some(ref since_ref) = since {
            match crate::impact::detect_changed_files_since(since_ref, &repo_root) {
                Ok(f) => f,
                Err(e) => {
                    return CallToolResult::error(format!("failed to list changed files: {e}"));
                }
            }
        } else {
            // Resolve relative path against repo root.
            if let Err(e) = validate_path(Path::new(&file), &repo_root) {
                return e;
            }
            vec![file]
        };

        if files.is_empty() {
            return CallToolResult::success("no changed files found".into());
        }

        // Load all embeddings once.
        let all_embeddings = match crate::embedding::load_all_embeddings(conn) {
            Ok(e) if !e.is_empty() => e,
            Ok(_) => {
                return CallToolResult::error(
                    "no embeddings found; run wonk_init with Ollama running to build embeddings"
                        .into(),
                );
            }
            Err(e) => return CallToolResult::error(format!("failed to load embeddings: {e}")),
        };

        let client = crate::embedding::OllamaClient::new();

        let mut all_results = Vec::new();
        for f in &files {
            match crate::impact::analyze_impact(conn, f, &repo_root, &client, &all_embeddings) {
                Ok(results) => all_results.extend(results),
                Err(e) => {
                    let msg = format!("{e:#}");
                    if msg.contains("unsupported language") {
                        continue;
                    }
                    return CallToolResult::error(format!("impact analysis failed: {e}"));
                }
            }
        }

        // Re-sort after merging results across multiple files.
        all_results.sort_by(|a, b| {
            b.similarity_score
                .partial_cmp(&a.similarity_score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        if all_results.is_empty() {
            return CallToolResult::success("no impact detected".into());
        }

        // Group by changed symbol.
        let changed_key = |r: &crate::types::ImpactResult| {
            format!(
                "{}:{}:{}:{}",
                r.changed_symbol.file,
                r.changed_symbol.line,
                r.changed_symbol.name,
                r.changed_symbol.kind
            )
        };

        let to_symbol_output = |s: &crate::types::SymbolRef| crate::output::ImpactSymbolOutput {
            name: s.name.clone(),
            kind: s.kind.to_string(),
            file: s.file.clone(),
            line: s.line,
        };

        let to_entry_output = |r: &crate::types::ImpactResult| crate::output::ImpactEntryOutput {
            file: r.impacted_symbol.file.clone(),
            line: r.impacted_symbol.line,
            symbol_name: r.impacted_symbol.name.clone(),
            symbol_kind: r.impacted_symbol.kind.to_string(),
            similarity_score: r.similarity_score,
        };

        let mut groups: Vec<(
            String,
            crate::output::ImpactSymbolOutput,
            Vec<crate::output::ImpactEntryOutput>,
        )> = Vec::new();

        for r in &all_results {
            let key = changed_key(r);
            if groups.last().is_some_and(|(k, _, _)| k == &key) {
                groups.last_mut().unwrap().2.push(to_entry_output(r));
            } else {
                groups.push((
                    key,
                    to_symbol_output(&r.changed_symbol),
                    vec![to_entry_output(r)],
                ));
            }
        }

        let outputs: Vec<crate::output::ImpactOutput> = groups
            .into_iter()
            .map(|(_, changed, impacted)| crate::output::ImpactOutput {
                changed_symbol: changed,
                impacted,
            })
            .collect();

        format_result(&outputs, format)
    }

    fn tool_update(&mut self, args: Value) -> CallToolResult {
        let format = extract_format(&args);
        let force = args.get("force").and_then(|v| v.as_bool()).unwrap_or(false);

        let repo_root = self.router.repo_root().to_path_buf();

        // Decide whether we need a full rebuild or can do incremental.
        let needs_full_rebuild = force
            || db::find_existing_index(&repo_root).is_none()
            || db::read_meta(&db::find_existing_index(&repo_root).unwrap_or_default())
                .ok()
                .and_then(|m| m.wonk_version)
                .as_deref()
                != Some(env!("CARGO_PKG_VERSION"));

        let (stats, emb_stats) = if needs_full_rebuild {
            let stats =
                match pipeline::rebuild_index_with_progress(&repo_root, false, &Progress::silent())
                {
                    Ok(s) => s,
                    Err(e) => return CallToolResult::error(format!("index rebuild failed: {e}")),
                };

            let emb_stats = db::index_path_for(&repo_root, false)
                .ok()
                .and_then(|p| db::open(&p).ok())
                .and_then(|conn| {
                    let client = crate::embedding::OllamaClient::new();
                    pipeline::build_embeddings(
                        &conn,
                        &repo_root,
                        &client,
                        crate::progress::ProgressMode::Silent,
                    )
                    .ok()
                })
                .unwrap_or(pipeline::EmbeddingBuildStats {
                    embedded_count: 0,
                    total_symbols: 0,
                    skipped: true,
                    elapsed: std::time::Duration::ZERO,
                });

            (stats, emb_stats)
        } else {
            let stats = match pipeline::incremental_update(&repo_root, false) {
                Ok(s) => s,
                Err(e) => return CallToolResult::error(format!("incremental update failed: {e}")),
            };

            let emb_stats = db::index_path_for(&repo_root, false)
                .ok()
                .and_then(|p| db::open(&p).ok())
                .and_then(|conn| {
                    let client = crate::embedding::OllamaClient::new();
                    pipeline::build_missing_embeddings(
                        &conn,
                        &repo_root,
                        &client,
                        crate::progress::ProgressMode::Silent,
                    )
                    .ok()
                })
                .unwrap_or(pipeline::EmbeddingBuildStats {
                    embedded_count: 0,
                    total_symbols: 0,
                    skipped: true,
                    elapsed: std::time::Duration::ZERO,
                });

            (stats, emb_stats)
        };

        // Refresh the router's connection to pick up the new index.
        self.router.refresh_connection();

        let result = serde_json::json!({
            "file_count": stats.file_count,
            "symbol_count": stats.symbol_count,
            "reference_count": stats.ref_count,
            "elapsed_ms": stats.elapsed.as_millis(),
            "embedding_count": emb_stats.embedded_count,
            "embedding_skipped": emb_stats.skipped
        });
        format_result(&result, format)
    }

    fn tool_delegate(&mut self, args: Value) -> CallToolResult {
        let question = match require_str(&args, "question") {
            Ok(q) => q,
            Err(e) => return e,
        };
        let scope = args.get("scope").and_then(|v| v.as_str());
        let format = extract_format(&args);

        let (conn, repo_root) = match self.resolve_repo(&args) {
            Ok(r) => r,
            Err(e) => return e,
        };

        let config = crate::config::Config::load(Some(&repo_root)).unwrap_or_default();

        let result = crate::delegate::delegate(conn, &repo_root, &config.llm, &question, scope);

        match result {
            Ok(dr) => {
                let out = serde_json::json!({
                    "answer": dr.answer,
                    "context_symbols": dr.context_symbols.iter().map(|cs| {
                        serde_json::json!({
                            "name": cs.name,
                            "kind": cs.kind,
                            "file": cs.file,
                            "line": cs.line,
                            "similarity": cs.similarity,
                        })
                    }).collect::<Vec<_>>(),
                });
                format_result(&out, format)
            }
            Err(e) => CallToolResult::error(format!("delegate failed: {e}")),
        }
    }
}

// ---------------------------------------------------------------------------
// Budget collection helper
// ---------------------------------------------------------------------------

/// Collect serializable outputs, applying optional token budget, and format the
/// final `CallToolResult`. Shared by tool_callers and tool_callees.
fn collect_with_budget<T: serde::Serialize>(
    outputs: Vec<T>,
    budget_limit: Option<usize>,
    format: OutputFormat,
) -> CallToolResult {
    if let Some(limit) = budget_limit {
        let mut budget = TokenBudget::new(limit);
        let mut kept: Vec<&T> = Vec::new();
        let mut truncated = 0usize;

        for out in &outputs {
            let serialized = serde_json::to_string(out).unwrap_or_default();
            if budget.try_consume(&serialized) {
                kept.push(out);
            } else {
                truncated += 1;
            }
        }

        if truncated > 0 {
            let shown = kept.len();
            let total = shown + truncated;
            let wrapper = serde_json::json!({
                "results": kept,
                "truncated": truncated,
                "hint": format!("Showing {shown} of {total} matches. Results are sorted by relevance."),
            });
            return format_result(&wrapper, format);
        }
    }

    format_result(&outputs, format)
}

// ---------------------------------------------------------------------------
// Serve loop
// ---------------------------------------------------------------------------

/// Run the MCP server, reading JSON-RPC from stdin and writing responses to stdout.
pub fn serve() -> Result<()> {
    let repo_root = db::find_repo_root(&std::env::current_dir()?)?;
    if db::find_existing_index(&repo_root).is_none() {
        pipeline::build_index(&repo_root, false)?;
    }

    // Discover all indexed repos at startup.
    let repos_dir = std::env::var("HOME")
        .map(|h| PathBuf::from(h).join(".wonk").join("repos"))
        .unwrap_or_default();
    let registry = RepoRegistry::new(discover_repos(&repos_dir));

    let mut server = McpServer::new(repo_root, registry);

    let stdin = io::stdin().lock();
    let mut stdout = io::stdout().lock();

    for line in stdin.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };

        if line.trim().is_empty() {
            continue;
        }

        let msg: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => {
                let resp = Response::error(RequestId::Number(0), PARSE_ERROR, "parse error");
                write_response(&mut stdout, &resp)?;
                continue;
            }
        };

        // Notifications have no `id` field — handle silently.
        let id = match msg.get("id") {
            Some(id_val) => match serde_json::from_value::<RequestId>(id_val.clone()) {
                Ok(id) => id,
                Err(_) => continue,
            },
            None => continue,
        };

        let method = match msg.get("method").and_then(|v| v.as_str()) {
            Some(m) => m,
            None => {
                let resp = Response::error(id, INVALID_REQUEST, "missing method");
                write_response(&mut stdout, &resp)?;
                continue;
            }
        };

        let empty_params = Value::Object(Default::default());
        let params = msg.get("params").unwrap_or(&empty_params);

        let resp = match method {
            "initialize" => Response::success(id, server.handle_initialize(params)),
            "ping" => Response::success(id, serde_json::json!({})),
            "tools/list" => Response::success(id, server.handle_tools_list()),
            "tools/call" => Response::success(id, server.handle_tools_call(params)),
            _ => Response::error(id, METHOD_NOT_FOUND, format!("unknown method: {method}")),
        };

        write_response(&mut stdout, &resp)?;
    }

    Ok(())
}

fn write_response(stdout: &mut impl Write, resp: &Response) -> io::Result<()> {
    let json = serde_json::to_string(resp).expect("serialize Response");
    writeln!(stdout, "{json}")?;
    stdout.flush()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Create a test server with no index.  Uses a non-existent path so the
    /// router cannot accidentally discover the real repo's index.
    fn test_server() -> McpServer {
        McpServer {
            router: QueryRouter::new(Some("/nonexistent/test/repo".into()), false),
            registry: RepoRegistry::new(Vec::new()),
        }
    }

    #[test]
    fn parse_request_with_number_id() {
        let json = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#;
        let v: Value = serde_json::from_str(json).unwrap();
        let id: RequestId = serde_json::from_value(v["id"].clone()).unwrap();
        assert!(matches!(id, RequestId::Number(1)));
    }

    #[test]
    fn parse_request_with_string_id() {
        let json = r#"{"jsonrpc":"2.0","id":"abc","method":"ping"}"#;
        let v: Value = serde_json::from_str(json).unwrap();
        let id: RequestId = serde_json::from_value(v["id"].clone()).unwrap();
        assert!(matches!(id, RequestId::Str(ref s) if s == "abc"));
    }

    #[test]
    fn notification_has_no_id() {
        let json = r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;
        let v: Value = serde_json::from_str(json).unwrap();
        assert!(v.get("id").is_none());
    }

    #[test]
    fn tool_definitions_count() {
        let tools = tool_definitions();
        assert_eq!(tools.len(), 23);
    }

    #[test]
    fn tool_definitions_have_valid_schemas() {
        let tools = tool_definitions();
        for tool in tools {
            assert!(
                tool.input_schema.get("type").is_some(),
                "tool {} missing schema type",
                tool.name
            );
        }
    }

    #[test]
    fn tool_definitions_are_cached() {
        let a = tool_definitions() as *const Vec<Tool>;
        let b = tool_definitions() as *const Vec<Tool>;
        assert_eq!(a, b);
    }

    #[test]
    fn initialize_response_has_correct_version() {
        let server = test_server();
        let result = server.handle_initialize(&Value::Object(Default::default()));
        assert_eq!(
            result["protocolVersion"].as_str().unwrap(),
            PROTOCOL_VERSION
        );
        assert_eq!(result["serverInfo"]["name"].as_str().unwrap(), "wonk");
    }

    #[test]
    fn unknown_tool_returns_error() {
        let mut server = test_server();
        let params = serde_json::json!({"name": "nonexistent", "arguments": {}});
        let result = server.handle_tools_call(&params);
        assert!(result["isError"].as_bool().unwrap_or(false));
    }

    #[test]
    fn response_serialization_success() {
        let resp = Response::success(RequestId::Number(1), serde_json::json!({"ok": true}));
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"result\""));
        assert!(!json.contains("\"error\""));
    }

    #[test]
    fn response_serialization_error() {
        let resp = Response::error(RequestId::Number(1), METHOD_NOT_FOUND, "not found");
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"error\""));
        assert!(!json.contains("\"result\""));
    }

    #[test]
    fn malformed_json_produces_parse_error() {
        let bad = "not json at all";
        let result: std::result::Result<Value, _> = serde_json::from_str(bad);
        assert!(result.is_err());
    }

    #[test]
    fn call_tool_result_success_format() {
        let result = CallToolResult::success("hello".to_string());
        assert!(!result.is_error);
        assert_eq!(result.content.len(), 1);
        assert_eq!(result.content[0].text, "hello");
        assert_eq!(result.content[0].type_, "text");
    }

    #[test]
    fn call_tool_result_error_format() {
        let result = CallToolResult::error("something broke".to_string());
        assert!(result.is_error);
        assert_eq!(result.content[0].text, "something broke");
    }

    #[test]
    fn require_str_returns_value_when_present() {
        let args = serde_json::json!({"name": "test"});
        assert_eq!(require_str(&args, "name").unwrap(), "test");
    }

    #[test]
    fn require_str_returns_error_when_missing() {
        let args = serde_json::json!({});
        let err = require_str(&args, "name").unwrap_err();
        assert!(err.is_error);
        assert!(err.content[0].text.contains("missing"));
    }

    #[test]
    fn format_result_json_produces_success() {
        let data = vec![1, 2, 3];
        let result = format_result(&data, OutputFormat::Json);
        assert!(!result.is_error);
        assert!(result.content[0].text.contains("["));
    }

    #[test]
    fn format_result_toon_produces_success() {
        let data = vec![1, 2, 3];
        let result = format_result(&data, OutputFormat::Toon);
        assert!(!result.is_error);
        assert!(!result.content[0].text.is_empty());
    }

    #[test]
    fn tool_show_definition_schema() {
        let tools = tool_definitions();
        let show_tool = tools.iter().find(|t| t.name == "wonk_show").unwrap();
        let props = show_tool.input_schema["properties"].as_object().unwrap();
        assert!(props.contains_key("name"), "missing 'name' property");
        assert!(props.contains_key("kind"), "missing 'kind' property");
        assert!(props.contains_key("file"), "missing 'file' property");
        assert!(props.contains_key("exact"), "missing 'exact' property");
        assert!(props.contains_key("shallow"), "missing 'shallow' property");
        assert!(props.contains_key("budget"), "missing 'budget' property");
        assert!(props.contains_key("format"), "missing 'format' property");
        let required = show_tool.input_schema["required"].as_array().unwrap();
        assert!(required.contains(&serde_json::json!("name")));
    }

    #[test]
    fn tool_show_dispatches_correctly() {
        // The test_server has no index, so wonk_show returns an error about
        // missing index — but it should dispatch to the correct handler
        // (not "unknown tool").
        let mut server = test_server();
        let params = serde_json::json!({
            "name": "wonk_show",
            "arguments": {"name": "nonexistent_symbol_xyz"}
        });
        let result = server.handle_tools_call(&params);
        let text = result["content"][0]["text"].as_str().unwrap_or("");
        // Should be the "no index" error, not "unknown tool".
        assert!(
            text.contains("no index") || text.contains("[]"),
            "expected 'no index' error or empty results, got: {text}"
        );
    }

    #[test]
    fn tool_show_budget_truncates() {
        use crate::pipeline;
        use tempfile::TempDir;

        let dir = TempDir::new().unwrap();
        let root = dir.path();
        std::fs::create_dir(root.join(".git")).unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(
            root.join("src/lib.rs"),
            "fn alpha() {\n    1\n}\nfn beta() {\n    2\n}\nfn gamma() {\n    3\n}\n",
        )
        .unwrap();

        pipeline::build_index(root, true).unwrap();
        let mut server = McpServer {
            router: QueryRouter::new(Some(root.to_path_buf()), true),
            registry: RepoRegistry::new(Vec::new()),
        };

        // Budget of 1 token (~4 chars) should truncate most results.
        let params = serde_json::json!({
            "name": "wonk_show",
            "arguments": {"name": "a", "budget": 1, "format": "json"}
        });
        let result = server.handle_tools_call(&params);
        let text = result["content"][0]["text"].as_str().unwrap_or("");
        let parsed: Value = serde_json::from_str(text).unwrap_or_default();

        // With a tiny budget, the response should be a wrapper with truncation metadata.
        if parsed.get("truncated").is_some() {
            assert!(parsed["truncated"].as_u64().unwrap() > 0);
            let hint = parsed["hint"]
                .as_str()
                .expect("truncated response should have a hint");
            assert!(
                hint.contains("Showing") && hint.contains("sorted by relevance"),
                "hint should use soft wording, got: {hint}"
            );
        } else {
            // If only one result fits, it's returned as a plain array — that's fine too.
            let arr = parsed.as_array().unwrap();
            assert!(arr.len() <= 1, "with budget=1, at most 1 result should fit");
        }
    }

    #[test]
    fn tool_status_includes_embedding_fields() {
        // The test_server has no index, so status will show indexed=false.
        // But the JSON should still contain the embedding fields.
        let mut server = test_server();
        let result = server.tool_status(serde_json::json!({}));
        assert!(!result.is_error);
        let text = &result.content[0].text;
        let json: Value = serde_json::from_str(text).unwrap();
        // Should contain the new embedding-related fields.
        assert!(
            json.get("embedding_count").is_some(),
            "missing embedding_count"
        );
        assert!(
            json.get("stale_embedding_count").is_some(),
            "missing stale_embedding_count"
        );
        assert!(
            json.get("ollama_reachable").is_some(),
            "missing ollama_reachable"
        );
    }

    // -- Callers/Callees MCP tests -------------------------------------------

    #[test]
    fn tools_list_returns_twenty_three_tools() {
        let server = test_server();
        let result = server.handle_tools_list();
        let tools = result["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 23);
    }

    #[test]
    fn tool_callers_definition_schema() {
        let tools = tool_definitions();
        let tool = tools.iter().find(|t| t.name == "wonk_callers").unwrap();
        let props = tool.input_schema["properties"].as_object().unwrap();
        assert!(props.contains_key("name"), "missing 'name' property");
        assert!(props.contains_key("depth"), "missing 'depth' property");
        assert!(props.contains_key("format"), "missing 'format' property");
        assert!(
            props.contains_key("min_confidence"),
            "missing 'min_confidence' property"
        );
        let required = tool.input_schema["required"].as_array().unwrap();
        assert!(required.contains(&serde_json::json!("name")));
    }

    #[test]
    fn tool_callees_definition_schema() {
        let tools = tool_definitions();
        let tool = tools.iter().find(|t| t.name == "wonk_callees").unwrap();
        let props = tool.input_schema["properties"].as_object().unwrap();
        assert!(props.contains_key("name"), "missing 'name' property");
        assert!(props.contains_key("depth"), "missing 'depth' property");
        assert!(props.contains_key("format"), "missing 'format' property");
        assert!(
            props.contains_key("min_confidence"),
            "missing 'min_confidence' property"
        );
        let required = tool.input_schema["required"].as_array().unwrap();
        assert!(required.contains(&serde_json::json!("name")));
    }

    #[test]
    fn tool_callers_dispatches_correctly() {
        let mut server = test_server();
        let params = serde_json::json!({
            "name": "wonk_callers",
            "arguments": {"name": "nonexistent_symbol_xyz"}
        });
        let result = server.handle_tools_call(&params);
        let text = result["content"][0]["text"].as_str().unwrap_or("");
        // Should be "no index" error, not "unknown tool".
        assert!(
            text.contains("no index") || text.contains("[]") || text.contains("call graph"),
            "expected index/callgraph error, got: {text}"
        );
    }

    #[test]
    fn tool_callees_dispatches_correctly() {
        let mut server = test_server();
        let params = serde_json::json!({
            "name": "wonk_callees",
            "arguments": {"name": "nonexistent_symbol_xyz"}
        });
        let result = server.handle_tools_call(&params);
        let text = result["content"][0]["text"].as_str().unwrap_or("");
        assert!(
            text.contains("no index") || text.contains("[]") || text.contains("call graph"),
            "expected index/callgraph error, got: {text}"
        );
    }

    #[test]
    fn tool_callpath_definition_schema() {
        let tools = tool_definitions();
        let tool = tools.iter().find(|t| t.name == "wonk_callpath").unwrap();
        let props = tool.input_schema["properties"].as_object().unwrap();
        assert!(props.contains_key("from"), "missing 'from' property");
        assert!(props.contains_key("to"), "missing 'to' property");
        assert!(props.contains_key("format"), "missing 'format' property");
        assert!(
            props.contains_key("min_confidence"),
            "missing 'min_confidence' property"
        );
        let required = tool.input_schema["required"].as_array().unwrap();
        assert!(required.contains(&serde_json::json!("from")));
        assert!(required.contains(&serde_json::json!("to")));
    }

    #[test]
    fn tool_callpath_dispatches_correctly() {
        let mut server = test_server();
        let params = serde_json::json!({
            "name": "wonk_callpath",
            "arguments": {"from": "nonexistent_xyz", "to": "nonexistent_abc"}
        });
        let result = server.handle_tools_call(&params);
        let text = result["content"][0]["text"].as_str().unwrap_or("");
        assert!(
            text.contains("no index") || text.contains("no path") || text.contains("call graph"),
            "expected index/callgraph error, got: {text}"
        );
    }

    // -- Summary tool tests ---------------------------------------------------

    #[test]
    fn tool_summary_definition_schema() {
        let tools = tool_definitions();
        let tool = tools.iter().find(|t| t.name == "wonk_summary").unwrap();
        let props = tool.input_schema["properties"].as_object().unwrap();
        assert!(props.contains_key("path"), "missing 'path' property");
        assert!(props.contains_key("depth"), "missing 'depth' property");
        assert!(
            props.contains_key("recursive"),
            "missing 'recursive' property"
        );
        assert!(props.contains_key("budget"), "missing 'budget' property");
        assert!(props.contains_key("format"), "missing 'format' property");
        let required = tool.input_schema["required"].as_array().unwrap();
        assert!(required.contains(&serde_json::json!("path")));
    }

    #[test]
    fn tool_summary_dispatches_correctly() {
        use tempfile::TempDir;

        // Use a real temp dir so path validation (canonicalize) succeeds,
        // but there is no index so we expect a "no index" error.
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        std::fs::create_dir(root.join(".git")).unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();

        let mut server = McpServer {
            router: QueryRouter::new(Some(root.to_path_buf()), true),
            registry: RepoRegistry::new(Vec::new()),
        };
        let params = serde_json::json!({
            "name": "wonk_summary",
            "arguments": {"path": "src/"}
        });
        let result = server.handle_tools_call(&params);
        let text = result["content"][0]["text"].as_str().unwrap_or("");
        assert!(
            text.contains("no index"),
            "expected 'no index' error, got: {text}"
        );
    }

    #[test]
    fn tool_summary_with_indexed_repo() {
        use crate::pipeline;
        use tempfile::TempDir;

        let dir = TempDir::new().unwrap();
        let root = dir.path();
        std::fs::create_dir(root.join(".git")).unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/lib.rs"), "fn hello() {}\nfn world() {}\n").unwrap();

        pipeline::build_index(root, true).unwrap();

        let mut server = McpServer {
            router: QueryRouter::new(Some(root.to_path_buf()), true),
            registry: RepoRegistry::new(Vec::new()),
        };
        let params = serde_json::json!({
            "name": "wonk_summary",
            "arguments": {"path": "src/", "format": "json"}
        });
        let result = server.handle_tools_call(&params);
        let text = result["content"][0]["text"].as_str().unwrap_or("");
        let v: Value = serde_json::from_str(text).expect("should be valid JSON");
        assert_eq!(v["path"], "src");
        assert_eq!(v["type"], "directory");
        assert_eq!(v["detail_level"], "outline");
        assert!(v["metrics"]["file_count"].as_u64().unwrap() > 0);
    }

    #[test]
    fn tool_summary_outline_detail() {
        use crate::pipeline;
        use tempfile::TempDir;

        let dir = TempDir::new().unwrap();
        let root = dir.path();
        std::fs::create_dir(root.join(".git")).unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/lib.rs"), "fn hello() {}\n").unwrap();

        pipeline::build_index(root, true).unwrap();

        let mut server = McpServer {
            router: QueryRouter::new(Some(root.to_path_buf()), true),
            registry: RepoRegistry::new(Vec::new()),
        };
        let params = serde_json::json!({
            "name": "wonk_summary",
            "arguments": {"path": "src/", "detail": "outline", "format": "json"}
        });
        let result = server.handle_tools_call(&params);
        let text = result["content"][0]["text"].as_str().unwrap_or("");
        let v: Value = serde_json::from_str(text).expect("should be valid JSON");
        assert_eq!(v["detail_level"], "outline");
        assert!(v["metrics"]["file_count"].is_number());
        // symbol_counts and dependency_count should be absent in outline mode
        assert!(v["metrics"].get("symbol_counts").is_none());
        assert!(v["metrics"].get("dependency_count").is_none());
    }

    #[test]
    fn tool_summary_with_depth() {
        use crate::pipeline;
        use tempfile::TempDir;

        let dir = TempDir::new().unwrap();
        let root = dir.path();
        std::fs::create_dir(root.join(".git")).unwrap();
        std::fs::create_dir_all(root.join("src/sub")).unwrap();
        std::fs::write(root.join("src/a.rs"), "fn alpha() {}\n").unwrap();
        std::fs::write(root.join("src/sub/b.rs"), "fn beta() {}\n").unwrap();

        pipeline::build_index(root, true).unwrap();

        let mut server = McpServer {
            router: QueryRouter::new(Some(root.to_path_buf()), true),
            registry: RepoRegistry::new(Vec::new()),
        };
        let params = serde_json::json!({
            "name": "wonk_summary",
            "arguments": {"path": "src/", "depth": 1, "format": "json"}
        });
        let result = server.handle_tools_call(&params);
        let text = result["content"][0]["text"].as_str().unwrap_or("");
        let v: Value = serde_json::from_str(text).expect("should be valid JSON");
        assert!(v["children"].is_array());
        assert!(!v["children"].as_array().unwrap().is_empty());
    }

    #[test]
    fn tool_summary_missing_path() {
        let mut server = test_server();
        let params = serde_json::json!({
            "name": "wonk_summary",
            "arguments": {}
        });
        let result = server.handle_tools_call(&params);
        let is_error = result["isError"].as_bool().unwrap_or(false);
        assert!(is_error, "should error on missing path");
    }

    // -- wonk_flows tests -----------------------------------------------------

    #[test]
    fn tool_flows_definition_schema() {
        let tools = tool_definitions();
        let tool = tools.iter().find(|t| t.name == "wonk_flows").unwrap();
        let props = tool.input_schema["properties"].as_object().unwrap();
        assert!(props.contains_key("entry"), "missing 'entry' property");
        assert!(props.contains_key("from"), "missing 'from' property");
        assert!(props.contains_key("depth"), "missing 'depth' property");
        assert!(
            props.contains_key("branching"),
            "missing 'branching' property"
        );
        assert!(
            props.contains_key("min_confidence"),
            "missing 'min_confidence' property"
        );
    }

    #[test]
    fn tool_flows_dispatches_correctly() {
        let mut server = test_server();
        let params = serde_json::json!({
            "name": "wonk_flows",
            "arguments": {"entry": "nonexistent_symbol_xyz"}
        });
        let result = server.handle_tools_call(&params);
        // Should not panic; returns a result (possibly empty or error).
        assert!(result.get("content").is_some() || result.get("isError").is_some());
    }

    #[test]
    fn tool_flows_list_mode() {
        let mut server = test_server();
        let params = serde_json::json!({
            "name": "wonk_flows",
            "arguments": {}
        });
        let result = server.handle_tools_call(&params);
        // List mode: should return a result (possibly empty list).
        assert!(result.get("content").is_some() || result.get("isError").is_some());
    }

    // -- wonk_blast tests -----------------------------------------------------

    #[test]
    fn tool_blast_definition_schema() {
        let tools = tool_definitions();
        let tool = tools.iter().find(|t| t.name == "wonk_blast").unwrap();
        let props = tool.input_schema["properties"].as_object().unwrap();
        assert!(props.contains_key("symbol"), "missing 'symbol' property");
        assert!(
            props.contains_key("direction"),
            "missing 'direction' property"
        );
        assert!(props.contains_key("depth"), "missing 'depth' property");
        assert!(
            props.contains_key("include_tests"),
            "missing 'include_tests' property"
        );
        assert!(
            props.contains_key("min_confidence"),
            "missing 'min_confidence' property"
        );
        assert!(props.contains_key("format"), "missing 'format' property");
        let required = tool.input_schema["required"].as_array().unwrap();
        assert!(required.contains(&serde_json::json!("symbol")));
    }

    #[test]
    fn tool_blast_dispatches_correctly() {
        let mut server = test_server();
        let params = serde_json::json!({
            "name": "wonk_blast",
            "arguments": {"symbol": "nonexistent_symbol_xyz"}
        });
        let result = server.handle_tools_call(&params);
        let text = result["content"][0]["text"].as_str().unwrap_or("");
        assert!(
            text.contains("no index")
                || text.contains("[]")
                || text.contains("call graph")
                || text.contains("blast")
                || text.contains("total_affected"),
            "expected index/callgraph error or blast result, got: {text}"
        );
    }

    #[test]
    fn tool_blast_missing_symbol() {
        let mut server = test_server();
        let params = serde_json::json!({
            "name": "wonk_blast",
            "arguments": {}
        });
        let result = server.handle_tools_call(&params);
        let is_error = result["isError"].as_bool().unwrap_or(false);
        assert!(is_error, "should error on missing symbol");
    }

    // -- wonk_changes tests (TASK-072) ----------------------------------------

    #[test]
    fn tool_changes_definition_exists() {
        let tools = tool_definitions();
        let tool = tools.iter().find(|t| t.name == "wonk_changes");
        assert!(tool.is_some(), "wonk_changes tool should exist");
    }

    #[test]
    fn tool_changes_definition_schema() {
        let tools = tool_definitions();
        let tool = tools.iter().find(|t| t.name == "wonk_changes").unwrap();
        let props = tool.input_schema["properties"].as_object().unwrap();
        assert!(props.contains_key("scope"), "missing 'scope' property");
        assert!(props.contains_key("base"), "missing 'base' property");
        assert!(props.contains_key("blast"), "missing 'blast' property");
        assert!(props.contains_key("flows"), "missing 'flows' property");
        assert!(
            props.contains_key("min_confidence"),
            "missing 'min_confidence' property"
        );
        assert!(props.contains_key("format"), "missing 'format' property");
    }

    // -- wonk_context tests (TASK-073) ----------------------------------------

    #[test]
    fn tool_context_definition_exists() {
        let tools = tool_definitions();
        let tool = tools.iter().find(|t| t.name == "wonk_context");
        assert!(tool.is_some(), "wonk_context tool should exist");
    }

    #[test]
    fn tool_context_definition_schema() {
        let tools = tool_definitions();
        let tool = tools.iter().find(|t| t.name == "wonk_context").unwrap();
        let props = tool.input_schema["properties"].as_object().unwrap();
        assert!(props.contains_key("name"), "missing 'name' property");
        assert!(props.contains_key("file"), "missing 'file' property");
        assert!(props.contains_key("kind"), "missing 'kind' property");
        assert!(
            props.contains_key("min_confidence"),
            "missing 'min_confidence' property"
        );
        assert!(props.contains_key("format"), "missing 'format' property");
        let required = tool.input_schema["required"].as_array().unwrap();
        assert!(required.contains(&serde_json::json!("name")));
    }

    #[test]
    fn tool_context_missing_name() {
        let mut server = test_server();
        let result = server.handle_tools_call(&serde_json::json!({
            "name": "wonk_context",
            "arguments": {}
        }));
        let call_result = &result["content"][0]["text"].as_str().unwrap();
        assert!(
            call_result.contains("missing"),
            "should report missing name parameter"
        );
    }

    // -- Multi-repo discovery tests (TASK-074) --------------------------------

    #[test]
    fn discover_repos_empty_dir() {
        let dir = tempfile::TempDir::new().unwrap();
        let repos_dir = dir.path().join("repos");
        std::fs::create_dir(&repos_dir).unwrap();

        let entries = discover_repos(&repos_dir);
        assert!(entries.is_empty());
    }

    #[test]
    fn discover_repos_nonexistent_dir() {
        let entries = discover_repos(Path::new("/tmp/nonexistent_wonk_test_dir"));
        assert!(entries.is_empty());
    }

    #[test]
    fn discover_repos_finds_indexed_repo() {
        let dir = tempfile::TempDir::new().unwrap();
        let repos_dir = dir.path().join("repos");
        let hash_dir = repos_dir.join("abcdef1234567890");
        std::fs::create_dir_all(&hash_dir).unwrap();

        // Create a minimal repo to index.
        let repo_dir = dir.path().join("my-project");
        std::fs::create_dir_all(repo_dir.join(".git")).unwrap();
        std::fs::create_dir_all(repo_dir.join("src")).unwrap();
        std::fs::write(repo_dir.join("src/lib.rs"), "fn hello() {}\n").unwrap();

        // Create index.db and meta.json in hash_dir.
        let index_path = hash_dir.join("index.db");
        let conn = db::open(&index_path).unwrap();
        drop(conn);
        db::write_meta(&index_path, &repo_dir, &["rust".to_string()]).unwrap();

        let entries = discover_repos(&repos_dir);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "my-project");
        assert_eq!(entries[0].repo_path, repo_dir);
        assert_eq!(entries[0].index_path, index_path);
        assert_eq!(entries[0].languages, vec!["rust".to_string()]);
        assert!(entries[0].created > 0);
    }

    #[test]
    fn discover_repos_skips_dir_without_index_db() {
        let dir = tempfile::TempDir::new().unwrap();
        let repos_dir = dir.path().join("repos");
        let hash_dir = repos_dir.join("abcdef1234567890");
        std::fs::create_dir_all(&hash_dir).unwrap();
        // Only meta.json, no index.db
        std::fs::write(
            hash_dir.join("meta.json"),
            r#"{"repo_path":"/tmp/foo","created":123,"languages":[]}"#,
        )
        .unwrap();

        let entries = discover_repos(&repos_dir);
        assert!(entries.is_empty());
    }

    #[test]
    fn registry_resolve_single_match() {
        let entries = vec![
            RepoEntry {
                repo_path: PathBuf::from("/home/user/projects/alpha"),
                index_path: PathBuf::from("/home/user/.wonk/repos/abc/index.db"),
                name: "alpha".to_string(),
                languages: vec![],
                created: 100,
            },
            RepoEntry {
                repo_path: PathBuf::from("/home/user/projects/beta"),
                index_path: PathBuf::from("/home/user/.wonk/repos/def/index.db"),
                name: "beta".to_string(),
                languages: vec![],
                created: 200,
            },
        ];
        let registry = RepoRegistry::new(entries);

        let resolved = registry.resolve("alpha").unwrap();
        assert_eq!(
            resolved.repo_path,
            PathBuf::from("/home/user/projects/alpha")
        );
        assert_eq!(
            resolved.index_path,
            PathBuf::from("/home/user/.wonk/repos/abc/index.db")
        );
    }

    #[test]
    fn registry_resolve_unknown_repo() {
        let entries = vec![RepoEntry {
            repo_path: PathBuf::from("/home/user/projects/alpha"),
            index_path: PathBuf::from("/home/user/.wonk/repos/abc/index.db"),
            name: "alpha".to_string(),
            languages: vec![],
            created: 100,
        }];
        let registry = RepoRegistry::new(entries);

        let err = registry.resolve("nonexistent").unwrap_err();
        assert!(err.contains("unknown repo"));
        assert!(err.contains("alpha"));
    }

    #[test]
    fn registry_resolve_ambiguous_name() {
        let entries = vec![
            RepoEntry {
                repo_path: PathBuf::from("/home/user/work/myapp"),
                index_path: PathBuf::from("/home/user/.wonk/repos/aaa/index.db"),
                name: "myapp".to_string(),
                languages: vec![],
                created: 100,
            },
            RepoEntry {
                repo_path: PathBuf::from("/home/user/personal/myapp"),
                index_path: PathBuf::from("/home/user/.wonk/repos/bbb/index.db"),
                name: "myapp".to_string(),
                languages: vec![],
                created: 200,
            },
        ];
        let registry = RepoRegistry::new(entries);

        let err = registry.resolve("myapp").unwrap_err();
        assert!(err.contains("ambiguous"));
        assert!(err.contains("/home/user/work/myapp"));
        assert!(err.contains("/home/user/personal/myapp"));
    }

    #[test]
    fn registry_lazy_connection_opens_and_caches() {
        let dir = tempfile::TempDir::new().unwrap();
        let index_path = dir.path().join("index.db");
        let conn = db::open(&index_path).unwrap();
        drop(conn);

        let mut registry = RepoRegistry::new(Vec::new());

        // First call should open the connection.
        assert!(registry.connections.is_empty());
        let result = registry.get_or_open_connection(&index_path);
        assert!(result.is_ok());
        assert_eq!(registry.connections.len(), 1);

        // Second call should reuse the cached connection.
        let result2 = registry.get_or_open_connection(&index_path);
        assert!(result2.is_ok());
        assert_eq!(registry.connections.len(), 1);
    }

    #[test]
    fn registry_lazy_connection_error_on_missing_db() {
        let mut registry = RepoRegistry::new(Vec::new());
        let result = registry.get_or_open_connection(Path::new("/tmp/nonexistent_wonk_test.db"));
        assert!(result.is_err());
    }

    #[test]
    fn all_tools_have_repo_param() {
        let tools = tool_definitions();
        for tool in tools.iter() {
            // wonk_repos lists all repos; wonk_init/wonk_update always target working directory.
            if tool.name == "wonk_repos" || tool.name == "wonk_init" || tool.name == "wonk_update" {
                continue;
            }
            let props = tool.input_schema["properties"].as_object().unwrap();
            assert!(
                props.contains_key("repo"),
                "tool {} is missing 'repo' property",
                tool.name
            );
            let repo_prop = &props["repo"];
            assert_eq!(
                repo_prop["type"].as_str().unwrap(),
                "string",
                "tool {} repo param should be string type",
                tool.name
            );
        }
    }

    #[test]
    fn repo_param_is_not_required() {
        let tools = tool_definitions();
        for tool in tools.iter() {
            if tool.name == "wonk_repos" || tool.name == "wonk_init" || tool.name == "wonk_update" {
                continue;
            }
            if let Some(required) = tool.input_schema.get("required") {
                if let Some(arr) = required.as_array() {
                    assert!(
                        !arr.contains(&serde_json::json!("repo")),
                        "tool {} should not require 'repo' param",
                        tool.name
                    );
                }
            }
        }
    }

    #[test]
    fn resolve_repo_defaults_to_working_dir() {
        // When no repo param, resolve_repo should return the default router's connection
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path();
        std::fs::create_dir(root.join(".git")).unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/lib.rs"), "fn hello() {}\n").unwrap();
        pipeline::build_index(root, true).unwrap();

        let mut server = McpServer {
            router: QueryRouter::new(Some(root.to_path_buf()), true),
            registry: RepoRegistry::new(Vec::new()),
        };

        let args = serde_json::json!({"name": "hello"});
        let resolved = server.resolve_repo(&args);
        assert!(
            resolved.is_ok(),
            "resolve_repo with no repo param should succeed"
        );
    }

    #[test]
    fn resolve_repo_with_unknown_repo_returns_error() {
        let mut server = test_server();
        let args = serde_json::json!({"repo": "nonexistent_project"});
        let resolved = server.resolve_repo(&args);
        assert!(resolved.is_err());
        let err = resolved.unwrap_err();
        assert!(err.is_error);
        assert!(err.content[0].text.contains("unknown repo"));
    }

    #[test]
    fn resolve_repo_with_valid_repo_opens_connection() {
        let dir = tempfile::TempDir::new().unwrap();
        let repo_dir = dir.path().join("my-project");
        std::fs::create_dir_all(repo_dir.join(".git")).unwrap();
        std::fs::create_dir_all(repo_dir.join("src")).unwrap();
        std::fs::write(repo_dir.join("src/lib.rs"), "fn hello() {}\n").unwrap();

        // Create an index in a fake repos dir.
        let repos_dir = dir.path().join("repos");
        let hash_dir = repos_dir.join("abc123");
        std::fs::create_dir_all(&hash_dir).unwrap();

        let index_path = hash_dir.join("index.db");
        let conn = db::open(&index_path).unwrap();
        drop(conn);
        db::write_meta(&index_path, &repo_dir, &["rust".to_string()]).unwrap();

        let entries = discover_repos(&repos_dir);
        let mut server = McpServer {
            router: QueryRouter::new(None, false),
            registry: RepoRegistry::new(entries),
        };

        let args = serde_json::json!({"repo": "my-project"});
        let resolved = server.resolve_repo(&args);
        assert!(
            resolved.is_ok(),
            "resolve_repo should succeed for registered repo"
        );
    }

    // -- wonk_repos tests (TASK-074) ------------------------------------------

    #[test]
    fn tool_repos_definition_exists() {
        let tools = tool_definitions();
        let tool = tools.iter().find(|t| t.name == "wonk_repos");
        assert!(tool.is_some(), "wonk_repos tool should exist");
    }

    #[test]
    fn tool_repos_dispatches_and_returns_empty_list() {
        let mut server = test_server();
        let params = serde_json::json!({
            "name": "wonk_repos",
            "arguments": {}
        });
        let result = server.handle_tools_call(&params);
        let text = result["content"][0]["text"].as_str().unwrap_or("");
        let parsed: Value = serde_json::from_str(text).unwrap();
        assert!(parsed.is_array(), "repos result should be an array");
        assert!(parsed.as_array().unwrap().is_empty());
    }

    #[test]
    fn tool_repos_returns_registered_repos_with_stats() {
        let dir = tempfile::TempDir::new().unwrap();
        let repo_dir = dir.path().join("my-project");
        std::fs::create_dir_all(repo_dir.join(".git")).unwrap();
        std::fs::create_dir_all(repo_dir.join("src")).unwrap();
        std::fs::write(
            repo_dir.join("src/lib.rs"),
            "fn hello() {}\nfn world() {}\n",
        )
        .unwrap();

        // Build a real index via pipeline into the central-style location.
        let repos_dir = dir.path().join("repos");
        let hash_dir = repos_dir.join("abc123");
        std::fs::create_dir_all(&hash_dir).unwrap();

        // First build a local index, then copy db + write meta to the repos dir.
        pipeline::build_index(&repo_dir, true).unwrap();
        let local_db = repo_dir.join(".wonk/index.db");
        let central_db = hash_dir.join("index.db");
        std::fs::copy(&local_db, &central_db).unwrap();
        db::write_meta(&central_db, &repo_dir, &["rust".to_string()]).unwrap();

        let entries = discover_repos(&repos_dir);
        let mut server = McpServer {
            router: QueryRouter::new(None, false),
            registry: RepoRegistry::new(entries),
        };

        let params = serde_json::json!({
            "name": "wonk_repos",
            "arguments": {}
        });
        let result = server.handle_tools_call(&params);
        let text = result["content"][0]["text"].as_str().unwrap_or("");
        let parsed: Value = serde_json::from_str(text).unwrap();
        let repos = parsed.as_array().unwrap();
        assert_eq!(repos.len(), 1);
        assert_eq!(repos[0]["name"].as_str().unwrap(), "my-project");
        assert!(repos[0]["path"].as_str().is_some());
        assert!(repos[0]["file_count"].as_u64().is_some());
        assert!(repos[0]["symbol_count"].as_u64().is_some());
        assert!(repos[0]["last_indexed"].as_u64().is_some());
    }

    // -- Cross-repo routing tests (TASK-074) ----------------------------------

    /// Helper: create a temp repo with source, build a local index, copy it to
    /// a central-style repos_dir, and return (repo_dir, repos_dir).
    fn setup_multi_repo_env() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let dir = tempfile::TempDir::new().unwrap();

        // Primary repo (working directory)
        let primary_dir = dir.path().join("primary-repo");
        std::fs::create_dir_all(primary_dir.join(".git")).unwrap();
        std::fs::create_dir_all(primary_dir.join("src")).unwrap();
        std::fs::write(primary_dir.join("src/lib.rs"), "fn primary_func() {}\n").unwrap();
        pipeline::build_index(&primary_dir, true).unwrap();

        // Other repo
        let other_dir = dir.path().join("other-project");
        std::fs::create_dir_all(other_dir.join(".git")).unwrap();
        std::fs::create_dir_all(other_dir.join("src")).unwrap();
        std::fs::write(
            other_dir.join("src/lib.rs"),
            "fn other_func() {}\nstruct OtherStruct {}\n",
        )
        .unwrap();
        pipeline::build_index(&other_dir, true).unwrap();

        // Copy other repo's index to central repos dir.
        let repos_dir = dir.path().join("repos");
        let hash_dir = repos_dir.join("other_hash");
        std::fs::create_dir_all(&hash_dir).unwrap();
        let local_db = other_dir.join(".wonk/index.db");
        let central_db = hash_dir.join("index.db");
        std::fs::copy(&local_db, &central_db).unwrap();
        db::write_meta(&central_db, &other_dir, &["rust".to_string()]).unwrap();

        (dir, primary_dir, repos_dir)
    }

    #[test]
    fn cross_repo_sym_query() {
        let (_dir, primary_dir, repos_dir) = setup_multi_repo_env();

        let entries = discover_repos(&repos_dir);
        let mut server = McpServer {
            router: QueryRouter::new(Some(primary_dir.to_path_buf()), true),
            registry: RepoRegistry::new(entries),
        };

        // Query default repo (should find primary_func).
        let params = serde_json::json!({
            "name": "wonk_sym",
            "arguments": {"name": "primary_func"}
        });
        let result = server.handle_tools_call(&params);
        let text = result["content"][0]["text"].as_str().unwrap_or("");
        let parsed: Value = serde_json::from_str(text).unwrap();
        let arr = parsed.as_array().unwrap();
        assert!(!arr.is_empty(), "should find primary_func in default repo");

        // Query other-project repo (should find other_func).
        let params = serde_json::json!({
            "name": "wonk_sym",
            "arguments": {"name": "other_func", "repo": "other-project"}
        });
        let result = server.handle_tools_call(&params);
        let text = result["content"][0]["text"].as_str().unwrap_or("");
        let parsed: Value = serde_json::from_str(text).unwrap();
        let arr = parsed.as_array().unwrap();
        assert!(
            !arr.is_empty(),
            "should find other_func in other-project repo"
        );
        assert_eq!(arr[0]["name"].as_str().unwrap(), "other_func");
    }

    #[test]
    fn cross_repo_ref_query() {
        let (_dir, primary_dir, repos_dir) = setup_multi_repo_env();

        let entries = discover_repos(&repos_dir);
        let mut server = McpServer {
            router: QueryRouter::new(Some(primary_dir.to_path_buf()), true),
            registry: RepoRegistry::new(entries),
        };

        // Query refs in other repo.
        let params = serde_json::json!({
            "name": "wonk_ref",
            "arguments": {"name": "OtherStruct", "repo": "other-project"}
        });
        let result = server.handle_tools_call(&params);
        // Should not be an error (might be empty array, but not "unknown tool").
        let is_error = result["isError"].as_bool().unwrap_or(false);
        assert!(!is_error, "ref query on other repo should not error");
    }

    #[test]
    fn cross_repo_status_query() {
        let (_dir, primary_dir, repos_dir) = setup_multi_repo_env();

        let entries = discover_repos(&repos_dir);
        let mut server = McpServer {
            router: QueryRouter::new(Some(primary_dir.to_path_buf()), true),
            registry: RepoRegistry::new(entries),
        };

        // Status for other-project.
        let params = serde_json::json!({
            "name": "wonk_status",
            "arguments": {"repo": "other-project"}
        });
        let result = server.handle_tools_call(&params);
        let text = result["content"][0]["text"].as_str().unwrap_or("");
        let parsed: Value = serde_json::from_str(text).unwrap();
        // Should show indexed=true for the other repo.
        assert_eq!(
            parsed["indexed"], true,
            "other repo should show indexed=true"
        );
    }

    #[test]
    fn cross_repo_unknown_repo_returns_error() {
        let mut server = test_server();
        let params = serde_json::json!({
            "name": "wonk_sym",
            "arguments": {"name": "foo", "repo": "nonexistent"}
        });
        let result = server.handle_tools_call(&params);
        let is_error = result["isError"].as_bool().unwrap_or(false);
        assert!(is_error, "unknown repo should return error");
        let text = result["content"][0]["text"].as_str().unwrap_or("");
        assert!(
            text.contains("unknown repo"),
            "error should mention unknown repo"
        );
    }

    // -- wonk_search no longer has semantic param ------------------------------

    #[test]
    fn tool_search_no_semantic_param() {
        let tools = tool_definitions();
        let tool = tools.iter().find(|t| t.name == "wonk_search").unwrap();
        let props = tool.input_schema["properties"].as_object().unwrap();
        assert!(
            !props.contains_key("semantic"),
            "wonk_search should not have 'semantic' property (moved to wonk_ask)"
        );
    }

    // -- wonk_ref output=files ------------------------------------------------

    #[test]
    fn tool_ref_has_output_param() {
        let tools = tool_definitions();
        let tool = tools.iter().find(|t| t.name == "wonk_ref").unwrap();
        let props = tool.input_schema["properties"].as_object().unwrap();
        assert!(
            props.contains_key("output"),
            "wonk_ref missing 'output' property"
        );
    }

    // -- wonk_ask tests -------------------------------------------------------

    #[test]
    fn tool_ask_definition_exists() {
        let tools = tool_definitions();
        let tool = tools.iter().find(|t| t.name == "wonk_ask");
        assert!(tool.is_some(), "wonk_ask tool should exist");
    }

    #[test]
    fn tool_ask_definition_schema() {
        let tools = tool_definitions();
        let tool = tools.iter().find(|t| t.name == "wonk_ask").unwrap();
        let props = tool.input_schema["properties"].as_object().unwrap();
        assert!(props.contains_key("query"), "missing 'query' property");
        assert!(props.contains_key("from"), "missing 'from' property");
        assert!(props.contains_key("to"), "missing 'to' property");
        assert!(props.contains_key("budget"), "missing 'budget' property");
        assert!(props.contains_key("format"), "missing 'format' property");
        assert!(props.contains_key("repo"), "missing 'repo' property");
        let required = tool.input_schema["required"].as_array().unwrap();
        assert!(required.contains(&serde_json::json!("query")));
    }

    #[test]
    fn tool_ask_dispatches_correctly() {
        let mut server = test_server();
        let params = serde_json::json!({
            "name": "wonk_ask",
            "arguments": {"query": "parse function"}
        });
        let result = server.handle_tools_call(&params);
        let text = result["content"][0]["text"].as_str().unwrap_or("");
        // Should get an error about missing index/embeddings, not "unknown tool".
        assert!(
            !text.contains("unknown tool"),
            "wonk_ask should dispatch correctly, got: {text}"
        );
    }

    // -- wonk_cluster tests ---------------------------------------------------

    #[test]
    fn tool_cluster_definition_exists() {
        let tools = tool_definitions();
        let tool = tools.iter().find(|t| t.name == "wonk_cluster");
        assert!(tool.is_some(), "wonk_cluster tool should exist");
    }

    #[test]
    fn tool_cluster_definition_schema() {
        let tools = tool_definitions();
        let tool = tools.iter().find(|t| t.name == "wonk_cluster").unwrap();
        let props = tool.input_schema["properties"].as_object().unwrap();
        assert!(props.contains_key("path"), "missing 'path' property");
        assert!(props.contains_key("top"), "missing 'top' property");
        assert!(props.contains_key("format"), "missing 'format' property");
        assert!(props.contains_key("repo"), "missing 'repo' property");
        let required = tool.input_schema["required"].as_array().unwrap();
        assert!(required.contains(&serde_json::json!("path")));
    }

    #[test]
    fn tool_cluster_dispatches_correctly() {
        let mut server = test_server();
        let params = serde_json::json!({
            "name": "wonk_cluster",
            "arguments": {"path": "src/"}
        });
        let result = server.handle_tools_call(&params);
        let text = result["content"][0]["text"].as_str().unwrap_or("");
        assert!(
            !text.contains("unknown tool"),
            "wonk_cluster should dispatch correctly, got: {text}"
        );
    }

    // -- wonk_impact tests ----------------------------------------------------

    #[test]
    fn tool_impact_definition_exists() {
        let tools = tool_definitions();
        let tool = tools.iter().find(|t| t.name == "wonk_impact");
        assert!(tool.is_some(), "wonk_impact tool should exist");
    }

    #[test]
    fn tool_impact_definition_schema() {
        let tools = tool_definitions();
        let tool = tools.iter().find(|t| t.name == "wonk_impact").unwrap();
        let props = tool.input_schema["properties"].as_object().unwrap();
        assert!(props.contains_key("file"), "missing 'file' property");
        assert!(props.contains_key("since"), "missing 'since' property");
        assert!(props.contains_key("format"), "missing 'format' property");
        assert!(props.contains_key("repo"), "missing 'repo' property");
        let required = tool.input_schema["required"].as_array().unwrap();
        assert!(required.contains(&serde_json::json!("file")));
    }

    #[test]
    fn tool_impact_dispatches_correctly() {
        let mut server = test_server();
        let params = serde_json::json!({
            "name": "wonk_impact",
            "arguments": {"file": "src/main.rs"}
        });
        let result = server.handle_tools_call(&params);
        let text = result["content"][0]["text"].as_str().unwrap_or("");
        assert!(
            !text.contains("unknown tool"),
            "wonk_impact should dispatch correctly, got: {text}"
        );
    }

    // -- wonk_update tests ----------------------------------------------------

    #[test]
    fn tool_update_definition_exists() {
        let tools = tool_definitions();
        let tool = tools.iter().find(|t| t.name == "wonk_update");
        assert!(tool.is_some(), "wonk_update tool should exist");
    }

    #[test]
    fn tool_update_excluded_from_repo_injection() {
        let tools = tool_definitions();
        let tool = tools.iter().find(|t| t.name == "wonk_update").unwrap();
        let props = tool.input_schema["properties"].as_object().unwrap();
        assert!(
            !props.contains_key("repo"),
            "wonk_update should not have 'repo' property"
        );
    }

    #[test]
    fn tool_update_dispatches_correctly() {
        let mut server = test_server();
        let params = serde_json::json!({
            "name": "wonk_update",
            "arguments": {}
        });
        let result = server.handle_tools_call(&params);
        let text = result["content"][0]["text"].as_str().unwrap_or("");
        assert!(
            !text.contains("unknown tool"),
            "wonk_update should dispatch correctly, got: {text}"
        );
    }
}
