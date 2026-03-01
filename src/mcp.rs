//! MCP (Model Context Protocol) server over stdio.
//!
//! Implements a JSON-RPC 2.0 server that exposes wonk's query capabilities
//! as MCP tools. Designed for use with AI coding assistants (Claude Code, etc.)
//! via the `wonk mcp serve` command.
//!
//! Transport: NDJSON over stdin/stdout. No async runtime required.

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
        vec![
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
        ]
    })
}

// ---------------------------------------------------------------------------
// MCP Server
// ---------------------------------------------------------------------------

struct McpServer {
    router: QueryRouter,
}

impl McpServer {
    fn new(repo_root: PathBuf) -> Self {
        let router = QueryRouter::new(Some(repo_root), false);
        Self { router }
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

    fn handle_tools_call(&self, params: &Value) -> Value {
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
            _ => CallToolResult::error(format!("unknown tool: {}", call.name)),
        };

        serde_json::to_value(result).expect("serialize CallToolResult")
    }

    // -- Tool handlers -------------------------------------------------------

    fn tool_search(&self, args: Value) -> CallToolResult {
        let query = match require_str(&args, "query") {
            Ok(q) => q,
            Err(e) => return e,
        };
        let regex = args.get("regex").and_then(|v| v.as_bool()).unwrap_or(false);
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
            .map(|v| v as usize);
        let format = extract_format(&args);

        let results = match search::text_search(&query, regex, case_insensitive, &paths) {
            Ok(r) => r,
            Err(_) => return CallToolResult::error("search failed".into()),
        };

        let groups = ranker::rank_and_dedup(&results, self.router.conn(), &query);

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

    fn tool_sym(&self, args: Value) -> CallToolResult {
        let name = match require_str(&args, "name") {
            Ok(n) => n,
            Err(e) => return e,
        };
        let kind = args.get("kind").and_then(|v| v.as_str());
        let exact = args.get("exact").and_then(|v| v.as_bool()).unwrap_or(false);
        let format = extract_format(&args);

        let results = match self.router.query_symbols(&name, kind, exact) {
            Ok(r) => r,
            Err(_) => return CallToolResult::error("symbol query failed".into()),
        };

        let outputs: Vec<SymbolOutput> = results.iter().map(symbol_to_output).collect();
        format_result(&outputs, format)
    }

    fn tool_ref(&self, args: Value) -> CallToolResult {
        let name = match require_str(&args, "name") {
            Ok(n) => n,
            Err(e) => return e,
        };
        let paths: Vec<String> = args
            .get("paths")
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or_default();
        let format = extract_format(&args);

        let results = match self.router.query_references(&name, &paths) {
            Ok(r) => r,
            Err(_) => return CallToolResult::error("reference query failed".into()),
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
            })
            .collect();

        format_result(&outputs, format)
    }

    fn tool_sig(&self, args: Value) -> CallToolResult {
        let name = match require_str(&args, "name") {
            Ok(n) => n,
            Err(e) => return e,
        };
        let format = extract_format(&args);

        let results = match self.router.query_signatures(&name) {
            Ok(r) => r,
            Err(_) => return CallToolResult::error("signature query failed".into()),
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

    fn tool_ls(&self, args: Value) -> CallToolResult {
        let path = args.get("path").and_then(|v| v.as_str()).unwrap_or(".");
        let tree = args.get("tree").and_then(|v| v.as_bool()).unwrap_or(false);
        let format = extract_format(&args);

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

    fn tool_deps(&self, args: Value) -> CallToolResult {
        let file = match require_str(&args, "file") {
            Ok(f) => f,
            Err(e) => return e,
        };
        let format = extract_format(&args);
        if validate_path(Path::new(&file), self.router.repo_root()).is_err() {
            return CallToolResult::error("path is outside the repository".into());
        }

        let results = match self.router.query_deps(&file) {
            Ok(r) => r,
            Err(_) => return CallToolResult::error("dependency query failed".into()),
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

    fn tool_rdeps(&self, args: Value) -> CallToolResult {
        let file = match require_str(&args, "file") {
            Ok(f) => f,
            Err(e) => return e,
        };
        let format = extract_format(&args);
        if validate_path(Path::new(&file), self.router.repo_root()).is_err() {
            return CallToolResult::error("path is outside the repository".into());
        }

        let results = match self.router.query_rdeps(&file) {
            Ok(r) => r,
            Err(_) => return CallToolResult::error("reverse dependency query failed".into()),
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

    fn tool_status(&self, args: Value) -> CallToolResult {
        let format = extract_format(&args);
        let info = crate::router::query_status_info(self.router.conn());
        let status = serde_json::to_value(&info).unwrap_or_default();
        format_result(&status, format)
    }

    fn tool_init(&self, args: Value) -> CallToolResult {
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

    fn tool_show(&self, args: Value) -> CallToolResult {
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

        let conn = match self.router.conn() {
            Some(c) => c,
            None => return CallToolResult::error("no index available; run wonk_init first".into()),
        };

        let options = crate::show::ShowOptions {
            file,
            kind,
            exact,
            suppress: true,
            shallow,
        };

        let results = match crate::show::show_symbol(conn, &name, self.router.repo_root(), &options)
        {
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
        &self,
        args: &Value,
    ) -> Result<(&Connection, usize, Option<usize>, OutputFormat), CallToolResult> {
        let depth_raw = args.get("depth").and_then(|v| v.as_u64()).unwrap_or(1) as usize;
        let (depth, _) = crate::callgraph::clamp_depth(depth_raw);
        let budget_limit: Option<usize> = args
            .get("budget")
            .and_then(|v| v.as_u64())
            .map(|v| v as usize);
        let format = extract_format(args);

        let conn = match self.router.conn() {
            Some(c) => c,
            None => {
                return Err(CallToolResult::error(
                    "no index available; run wonk_init first".into(),
                ));
            }
        };

        if !crate::callgraph::has_caller_id_data(conn) {
            return Err(CallToolResult::error(
                "index lacks call graph data; run wonk_init to re-index".into(),
            ));
        }

        Ok((conn, depth, budget_limit, format))
    }

    fn tool_callers(&self, args: Value) -> CallToolResult {
        let name = match require_str(&args, "name") {
            Ok(n) => n,
            Err(e) => return e,
        };
        let (conn, depth, budget_limit, format) = match self.callgraph_setup(&args) {
            Ok(setup) => setup,
            Err(e) => return e,
        };

        let min_confidence: Option<f64> = args.get("min_confidence").and_then(|v| v.as_f64());

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

    fn tool_callees(&self, args: Value) -> CallToolResult {
        let name = match require_str(&args, "name") {
            Ok(n) => n,
            Err(e) => return e,
        };
        let (conn, depth, budget_limit, format) = match self.callgraph_setup(&args) {
            Ok(setup) => setup,
            Err(e) => return e,
        };

        let min_confidence: Option<f64> = args.get("min_confidence").and_then(|v| v.as_f64());

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

    fn tool_callpath(&self, args: Value) -> CallToolResult {
        let from = match require_str(&args, "from") {
            Ok(n) => n,
            Err(e) => return e,
        };
        let to = match require_str(&args, "to") {
            Ok(n) => n,
            Err(e) => return e,
        };
        let format = extract_format(&args);

        let conn = match self.router.conn() {
            Some(c) => c,
            None => {
                return CallToolResult::error("no index available; run wonk_init first".into());
            }
        };

        if !crate::callgraph::has_caller_id_data(conn) {
            return CallToolResult::error(
                "index lacks call graph data; run wonk_init to re-index".into(),
            );
        }

        let min_confidence: Option<f64> = args.get("min_confidence").and_then(|v| v.as_f64());

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

    fn tool_summary(&self, args: Value) -> CallToolResult {
        let path = match require_str(&args, "path") {
            Ok(p) => p,
            Err(e) => return e,
        };

        // Validate path stays within repo boundary.
        if let Err(e) = validate_path(Path::new(&path), self.router.repo_root()) {
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

        let conn = match self.router.conn() {
            Some(c) => c,
            None => return CallToolResult::error("no index available; run wonk_init first".into()),
        };

        let semantic_flag = args
            .get("semantic")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let semantic = if semantic_flag {
            let repo_root = self.router.repo_root();
            let config = crate::config::Config::load(Some(repo_root)).unwrap_or_default();
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

    let server = McpServer::new(repo_root);

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
        assert_eq!(tools.len(), 14);
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
        let server = test_server();
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
        let server = test_server();
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
        let server = McpServer {
            router: QueryRouter::new(Some(root.to_path_buf()), true),
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
        let server = test_server();
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
    fn tools_list_returns_fourteen_tools() {
        let server = test_server();
        let result = server.handle_tools_list();
        let tools = result["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 14);
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
        let server = test_server();
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
        let server = test_server();
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
        let server = test_server();
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
        let server = test_server();
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

        let server = McpServer {
            router: QueryRouter::new(Some(root.to_path_buf()), true),
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

        let server = McpServer {
            router: QueryRouter::new(Some(root.to_path_buf()), true),
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

        let server = McpServer {
            router: QueryRouter::new(Some(root.to_path_buf()), true),
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
        let server = test_server();
        let params = serde_json::json!({
            "name": "wonk_summary",
            "arguments": {}
        });
        let result = server.handle_tools_call(&params);
        let is_error = result["isError"].as_bool().unwrap_or(false);
        assert!(is_error, "should error on missing path");
    }
}
