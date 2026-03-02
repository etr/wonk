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
                description: "Full-text search across the codebase with structural ranking. Results are classified (definition > call site > import > other > comment > test) and deduplicated.",
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "query": {
                            "type": "string",
                            "description": "Search pattern (literal text or regex if regex=true)"
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
                description: "Look up symbol definitions (functions, classes, structs, etc.) by name.",
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
                description: "Find references (usages) of a symbol across the codebase.",
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
                description: "Show function/method signatures by name.",
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
                name: "wonk_ls",
                description: "List symbols defined in a file or directory.",
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "File or directory path (defaults to repo root)",
                            "default": "."
                        },
                        "tree": {
                            "type": "boolean",
                            "description": "Show symbols in a tree structure grouped by scope",
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
                name: "wonk_deps",
                description: "Show dependencies of a file (files it imports/uses).",
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
                description: "Show reverse dependencies (files that depend on a given file).",
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
                description: "Show index status: whether an index exists, file count, symbol count, reference count, embedding count, stale embedding count, and Ollama reachability.",
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
                description: "Initialize or rebuild the structural index and embeddings for the current repository. Embedding generation requires Ollama; if unavailable, only the structural index is built.",
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
                description: "Show full source body of a symbol. For container types (class, struct, enum, trait, interface), use shallow=true to get the container signature plus child signatures without bodies.",
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
                description: "Find all callers of a symbol (functions whose bodies reference it). Supports transitive expansion via depth parameter.",
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
                description: "Find all callees of a symbol (symbols referenced within its body). Supports transitive expansion via depth parameter.",
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
                description: "Find a call chain between two symbols via BFS traversal. Returns the shortest path from the source symbol to the target symbol.",
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
                description: "Show a structural summary of a file or directory: file count, line count, symbol counts by kind, language breakdown, and dependency count.",
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Path to summarize (file or directory, relative to repo root)"
                        },
                        "detail": {
                            "type": "string",
                            "enum": ["rich", "light", "symbols"],
                            "description": "Detail level: rich (all metrics), light (file count, symbol count, languages), symbols (symbol counts by kind only)",
                            "default": "rich"
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
                        "semantic": {
                            "type": "boolean",
                            "description": "Include AI-generated description via Ollama LLM (requires Ollama running)",
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
                description: "Detect entry points (functions/methods with no callers) and trace execution flows via BFS callee expansion. Without an entry parameter, lists all detected entry points. With an entry parameter, traces the full execution flow from that function.",
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
                description: "Analyze the blast radius of a symbol change. Shows all affected symbols grouped by severity tier (WILL BREAK, LIKELY AFFECTED, MAY NEED TESTING) with a risk level assessment. Supports upstream (callers) and downstream (callees) traversal with inheritance integration.",
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
                description: "Detect changed symbols in working tree. Optionally chain blast radius analysis and execution flow detection for each changed symbol. Supports scoping to unstaged, staged, all uncommitted, or compare-to-ref changes.",
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
                description: "Aggregate full context for a symbol: definition, categorized incoming references (callers, importers, type users), outgoing references (callees, imports), flow participation, and children (extending/implementing types). Returns one context block per matching symbol.",
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
        ];

        // Inject optional `repo` parameter into all existing tools.
        for tool in &mut tools {
            if let Some(props) = tool.input_schema.get_mut("properties")
                && let Some(obj) = props.as_object_mut()
            {
                obj.insert("repo".to_string(), repo_prop.clone());
            }
        }

        // Add the wonk_repos tool (does not get repo param — it lists all repos).
        tools.push(Tool {
            name: "wonk_repos",
            description: "List all indexed repositories available for querying. Returns name, path, file count, symbol count, and last indexed time for each repo.",
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
#[allow(dead_code)]
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
#[allow(dead_code)]
struct RepoRegistry {
    entries: Vec<RepoEntry>,
    /// Lazy-opened connections keyed by index_path string.
    connections: HashMap<String, Connection>,
}

/// Result of resolving a repo reference.
#[derive(Debug)]
#[allow(dead_code)]
struct ResolvedRepo {
    index_path: PathBuf,
    repo_path: PathBuf,
    name: String,
}

#[allow(dead_code)]
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
                    name: entry.name.clone(),
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
/// Scans `repos_dir/*/meta.json` to find all indexed repos and reads their
/// metadata. Also checks for a local index at `<repo_root>/.wonk/index.db`.
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
    #[allow(dead_code)]
    default_repo_root: PathBuf,
    #[allow(dead_code)]
    registry: RepoRegistry,
}

impl McpServer {
    fn new(repo_root: PathBuf, registry: RepoRegistry) -> Self {
        let router = QueryRouter::new(Some(repo_root.clone()), false);
        Self {
            router,
            default_repo_root: repo_root,
            registry,
        }
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

    /// Check if a `repo` param is present in the args.
    fn has_repo_param(args: &Value) -> bool {
        args.get("repo").and_then(|v| v.as_str()).is_some()
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
            "wonk_ls" => self.tool_ls(call.arguments),
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
        let regex = args.get("regex").and_then(|v| v.as_bool()).unwrap_or(false);
        let case_insensitive = args
            .get("case_insensitive")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let mut paths: Vec<String> = args
            .get("paths")
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or_default();
        let budget_limit: Option<usize> = args
            .get("budget")
            .and_then(|v| v.as_u64())
            .map(|v| v as usize);
        let format = extract_format(&args);

        // For cross-repo search, set the search path to the target repo root.
        let ranker_conn: Option<&Connection> = if Self::has_repo_param(&args) {
            let (conn, repo_root) = match self.resolve_repo(&args) {
                Ok(r) => r,
                Err(e) => return e,
            };
            if paths.is_empty() {
                paths.push(repo_root.to_string_lossy().into_owned());
            }
            Some(conn)
        } else {
            self.router.conn()
        };

        let results = match search::text_search(&query, regex, case_insensitive, &paths) {
            Ok(r) => r,
            Err(_) => return CallToolResult::error("search failed".into()),
        };

        let groups = ranker::rank_and_dedup(&results, ranker_conn, &query);

        let mut budget = budget_limit.map(TokenBudget::new);
        let mut outputs: Vec<SearchOutput> = Vec::new();

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
                        continue;
                    }
                    let serialized = serde_json::to_string(&out).unwrap_or_default();
                    if !b.try_consume(&serialized) {
                        continue;
                    }
                }
                outputs.push(out);
            }
        }

        format_result(&outputs, format)
    }

    fn tool_sym(&mut self, args: Value) -> CallToolResult {
        let name = match require_str(&args, "name") {
            Ok(n) => n,
            Err(e) => return e,
        };
        let kind = args.get("kind").and_then(|v| v.as_str());
        let exact = args.get("exact").and_then(|v| v.as_bool()).unwrap_or(false);
        let format = extract_format(&args);

        let results = if args.get("repo").and_then(|v| v.as_str()).is_some() {
            let (conn, _repo_root) = match self.resolve_repo(&args) {
                Ok(r) => r,
                Err(e) => return e,
            };
            crate::router::query_symbols_db(conn, &name, kind, exact).map_err(|e| e.to_string())
        } else {
            self.router
                .query_symbols(&name, kind, exact)
                .map_err(|e| e.to_string())
        };

        match results {
            Ok(r) => {
                let outputs: Vec<SymbolOutput> = r.iter().map(symbol_to_output).collect();
                format_result(&outputs, format)
            }
            Err(_) => CallToolResult::error("symbol query failed".into()),
        }
    }

    fn tool_ref(&mut self, args: Value) -> CallToolResult {
        let name = match require_str(&args, "name") {
            Ok(n) => n,
            Err(e) => return e,
        };
        let paths: Vec<String> = args
            .get("paths")
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or_default();
        let format = extract_format(&args);

        let results = if Self::has_repo_param(&args) {
            let (conn, _) = match self.resolve_repo(&args) {
                Ok(r) => r,
                Err(e) => return e,
            };
            match crate::router::query_references_db(conn, &name) {
                Ok(r) => r,
                Err(_) => return CallToolResult::error("reference query failed".into()),
            }
        } else {
            match self.router.query_references(&name, &paths) {
                Ok(r) => r,
                Err(_) => return CallToolResult::error("reference query failed".into()),
            }
        };

        let outputs: Vec<RefOutput> = results
            .iter()
            .map(|r| RefOutput {
                name: r.name.clone(),
                kind: r.kind.to_string(),
                file: r.file.clone(),
                line: r.line,
                col: r.col,
                context: r.context.clone(),
                caller_name: r.caller_name.clone(),
                confidence: r.confidence,
            })
            .collect();

        format_result(&outputs, format)
    }

    fn tool_sig(&mut self, args: Value) -> CallToolResult {
        let name = match require_str(&args, "name") {
            Ok(n) => n,
            Err(e) => return e,
        };
        let format = extract_format(&args);

        let results = if Self::has_repo_param(&args) {
            let (conn, _) = match self.resolve_repo(&args) {
                Ok(r) => r,
                Err(e) => return e,
            };
            match crate::router::query_signatures_db(conn, &name) {
                Ok(r) => r,
                Err(_) => return CallToolResult::error("signature query failed".into()),
            }
        } else {
            match self.router.query_signatures(&name) {
                Ok(r) => r,
                Err(_) => return CallToolResult::error("signature query failed".into()),
            }
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

    fn tool_ls(&mut self, args: Value) -> CallToolResult {
        let path = args.get("path").and_then(|v| v.as_str()).unwrap_or(".");
        let tree = args.get("tree").and_then(|v| v.as_bool()).unwrap_or(false);
        let format = extract_format(&args);

        if Self::has_repo_param(&args) {
            let (conn, repo_root) = match self.resolve_repo(&args) {
                Ok(r) => r,
                Err(e) => return e,
            };
            let path_buf = match validate_path(Path::new(path), &repo_root) {
                Ok(p) => p,
                Err(e) => return e,
            };
            let files: Vec<String> = if path_buf.is_dir() {
                let walker = crate::walker::Walker::new(&path_buf);
                walker
                    .collect_paths()
                    .into_iter()
                    .filter(|p| p.is_file())
                    .map(|p| p.to_string_lossy().into_owned())
                    .collect()
            } else {
                vec![path_buf.to_string_lossy().into_owned()]
            };
            let mut all_symbols = Vec::new();
            for file in &files {
                match crate::router::query_symbols_in_file_db(conn, file) {
                    Ok(syms) => all_symbols.extend(syms),
                    Err(_) => return CallToolResult::error("symbol listing failed".into()),
                }
            }
            let outputs: Vec<SymbolOutput> = all_symbols.iter().map(symbol_to_output).collect();
            return format_result(&outputs, format);
        }

        let path_buf = match validate_path(Path::new(path), self.router.repo_root()) {
            Ok(p) => p,
            Err(e) => return e,
        };

        let files: Vec<String> = if path_buf.is_dir() {
            let walker = crate::walker::Walker::new(&path_buf);
            walker
                .collect_paths()
                .into_iter()
                .filter(|p| p.is_file())
                .map(|p| p.to_string_lossy().into_owned())
                .collect()
        } else {
            vec![path_buf.to_string_lossy().into_owned()]
        };

        let mut all_symbols = Vec::new();
        for file in &files {
            match self.router.query_symbols_in_file(file, tree) {
                Ok(syms) => all_symbols.extend(syms),
                Err(_) => return CallToolResult::error("symbol listing failed".into()),
            }
        }

        let outputs: Vec<SymbolOutput> = all_symbols.iter().map(symbol_to_output).collect();
        format_result(&outputs, format)
    }

    fn tool_deps(&mut self, args: Value) -> CallToolResult {
        let file = match require_str(&args, "file") {
            Ok(f) => f,
            Err(e) => return e,
        };
        let format = extract_format(&args);

        let results = if Self::has_repo_param(&args) {
            let (conn, repo_root) = match self.resolve_repo(&args) {
                Ok(r) => r,
                Err(e) => return e,
            };
            if validate_path(Path::new(&file), &repo_root).is_err() {
                return CallToolResult::error("path is outside the repository".into());
            }
            match crate::router::query_deps_db(conn, &file) {
                Ok(r) => r,
                Err(_) => return CallToolResult::error("dependency query failed".into()),
            }
        } else {
            if validate_path(Path::new(&file), self.router.repo_root()).is_err() {
                return CallToolResult::error("path is outside the repository".into());
            }
            match self.router.query_deps(&file) {
                Ok(r) => r,
                Err(_) => return CallToolResult::error("dependency query failed".into()),
            }
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

        let results = if Self::has_repo_param(&args) {
            let (conn, repo_root) = match self.resolve_repo(&args) {
                Ok(r) => r,
                Err(e) => return e,
            };
            if validate_path(Path::new(&file), &repo_root).is_err() {
                return CallToolResult::error("path is outside the repository".into());
            }
            match crate::router::query_rdeps_db(conn, &file) {
                Ok(r) => r,
                Err(_) => return CallToolResult::error("reverse dependency query failed".into()),
            }
        } else {
            if validate_path(Path::new(&file), self.router.repo_root()).is_err() {
                return CallToolResult::error("path is outside the repository".into());
            }
            match self.router.query_rdeps(&file) {
                Ok(r) => r,
                Err(_) => return CallToolResult::error("reverse dependency query failed".into()),
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
        let conn = if Self::has_repo_param(&args) {
            let (c, _) = match self.resolve_repo(&args) {
                Ok(r) => r,
                Err(e) => return e,
            };
            Some(c)
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
            Err(_) => return CallToolResult::error("index build failed".into()),
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
        let name = match require_str(&args, "name") {
            Ok(n) => n,
            Err(e) => return e,
        };
        let kind = args.get("kind").and_then(|v| v.as_str()).map(String::from);
        let file = args.get("file").and_then(|v| v.as_str()).map(String::from);
        let exact = args.get("exact").and_then(|v| v.as_bool()).unwrap_or(false);
        let shallow = args
            .get("shallow")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let budget_limit: Option<usize> = args
            .get("budget")
            .and_then(|v| v.as_u64())
            .map(|v| v as usize);
        let format = extract_format(&args);

        let (conn, repo_root) = match self.resolve_repo(&args) {
            Ok(r) => r,
            Err(e) => return e,
        };

        let options = crate::show::ShowOptions {
            file,
            kind,
            exact,
            suppress: true,
            shallow,
        };

        let results = match crate::show::show_symbol(conn, &name, &repo_root, &options) {
            Ok(r) => r,
            Err(e) => return CallToolResult::error(format!("show query failed: {e}")),
        };

        let mut budget = budget_limit.map(TokenBudget::new);
        let mut outputs: Vec<ShowOutput> = Vec::new();
        let mut truncated = 0usize;

        for sr in &results {
            let out = ShowOutput::from(sr);

            if let Some(ref mut b) = budget {
                let serialized = serde_json::to_string(&out).unwrap_or_default();
                if !b.try_consume(&serialized) {
                    truncated += 1;
                    continue;
                }
            }
            outputs.push(out);
        }

        if truncated > 0 {
            // Append truncation metadata as a wrapper.
            let wrapper = serde_json::json!({
                "results": outputs,
                "truncated": truncated,
                "budget_limit": budget_limit,
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
            .map(|v| v as usize);
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

        let min_confidence: Option<f64> =
            args.get("min_confidence")
                .and_then(|v| v.as_f64())
                .map(|c| {
                    if c.is_nan() || c.is_infinite() {
                        0.0
                    } else {
                        c.clamp(0.0, 1.0)
                    }
                });

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

        let min_confidence: Option<f64> =
            args.get("min_confidence")
                .and_then(|v| v.as_f64())
                .map(|c| {
                    if c.is_nan() || c.is_infinite() {
                        0.0
                    } else {
                        c.clamp(0.0, 1.0)
                    }
                });

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

        let min_confidence: Option<f64> =
            args.get("min_confidence")
                .and_then(|v| v.as_f64())
                .map(|c| {
                    if c.is_nan() || c.is_infinite() {
                        0.0
                    } else {
                        c.clamp(0.0, 1.0)
                    }
                });

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

        let detail_str = args
            .get("detail")
            .and_then(|v| v.as_str())
            .unwrap_or("rich");
        let detail = match detail_str.parse::<crate::types::DetailLevel>() {
            Ok(d) => d,
            Err(e) => return CallToolResult::error(e),
        };

        let recursive = args
            .get("recursive")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let depth_raw = args.get("depth").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
        let depth = if recursive { None } else { Some(depth_raw) };

        let format = extract_format(&args);

        let semantic_flag = args
            .get("semantic")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let semantic = if semantic_flag {
            let config = crate::config::Config::load(Some(&repo_root)).unwrap_or_default();
            if let Err(e) = crate::db::ensure_summaries_table(conn) {
                return CallToolResult::error(format!("failed to create summaries table: {e}"));
            }
            Some(config.llm)
        } else {
            None
        };

        let options = crate::summary::SummaryOptions {
            detail,
            depth,
            suppress: true,
            semantic,
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
            let wrapper = serde_json::json!({
                "results": kept,
                "truncated": truncated,
                "budget_limit": budget_limit,
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

    fn test_server() -> McpServer {
        McpServer {
            router: QueryRouter::new(None, false),
            default_repo_root: PathBuf::from("."),
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
        assert_eq!(tools.len(), 19);
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
            default_repo_root: root.to_path_buf(),
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
            assert!(parsed["budget_limit"].as_u64().unwrap() == 1);
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
    fn tools_list_returns_nineteen_tools() {
        let server = test_server();
        let result = server.handle_tools_list();
        let tools = result["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 19);
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
        assert!(props.contains_key("detail"), "missing 'detail' property");
        assert!(props.contains_key("depth"), "missing 'depth' property");
        assert!(
            props.contains_key("recursive"),
            "missing 'recursive' property"
        );
        assert!(
            props.contains_key("semantic"),
            "missing 'semantic' property"
        );
        assert!(props.contains_key("budget"), "missing 'budget' property");
        assert!(props.contains_key("format"), "missing 'format' property");
        let required = tool.input_schema["required"].as_array().unwrap();
        assert!(required.contains(&serde_json::json!("path")));
    }

    #[test]
    fn tool_summary_dispatches_correctly() {
        let mut server = test_server();
        let params = serde_json::json!({
            "name": "wonk_summary",
            "arguments": {"path": "src/"}
        });
        let result = server.handle_tools_call(&params);
        let text = result["content"][0]["text"].as_str().unwrap_or("");
        // test_server has no index, so we expect a "no index" error
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
            default_repo_root: root.to_path_buf(),
            registry: RepoRegistry::new(Vec::new()),
        };
        let params = serde_json::json!({
            "name": "wonk_summary",
            "arguments": {"path": "src/", "detail": "rich", "format": "json"}
        });
        let result = server.handle_tools_call(&params);
        let text = result["content"][0]["text"].as_str().unwrap_or("");
        let v: Value = serde_json::from_str(text).expect("should be valid JSON");
        assert_eq!(v["path"], "src");
        assert_eq!(v["type"], "directory");
        assert_eq!(v["detail_level"], "rich");
        assert!(v["metrics"]["file_count"].as_u64().unwrap() > 0);
    }

    #[test]
    fn tool_summary_light_detail() {
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
            default_repo_root: root.to_path_buf(),
            registry: RepoRegistry::new(Vec::new()),
        };
        let params = serde_json::json!({
            "name": "wonk_summary",
            "arguments": {"path": "src/", "detail": "light", "format": "json"}
        });
        let result = server.handle_tools_call(&params);
        let text = result["content"][0]["text"].as_str().unwrap_or("");
        let v: Value = serde_json::from_str(text).expect("should be valid JSON");
        assert_eq!(v["detail_level"], "light");
        assert!(v["metrics"]["file_count"].is_number());
        // line_count and dependency_count should be absent in light mode
        assert!(v["metrics"].get("line_count").is_none());
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
            default_repo_root: root.to_path_buf(),
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
            // wonk_repos itself doesn't need a repo param
            if tool.name == "wonk_repos" {
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
            if tool.name == "wonk_repos" {
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
            default_repo_root: root.to_path_buf(),
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
            default_repo_root: PathBuf::from("."),
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
            default_repo_root: PathBuf::from("."),
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
            default_repo_root: primary_dir.to_path_buf(),
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
            default_repo_root: primary_dir.to_path_buf(),
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
            default_repo_root: primary_dir.to_path_buf(),
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
}
