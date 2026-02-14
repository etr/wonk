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
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::budget::TokenBudget;
use crate::db;
use crate::output::{
    DepOutput, OutputFormat, RefOutput, SearchOutput, SignatureOutput, SymbolOutput,
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
    let canonical = resolved.canonicalize().or_else(|_| {
        resolved
            .parent()
            .and_then(|p| p.canonicalize().ok())
            .map(|p| {
                p.join(
                    resolved
                        .file_name()
                        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid path"))
                        .unwrap_or_default(),
                )
            })
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "path not found"))
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
                description: "Show index status: whether an index exists, file count, symbol count, and reference count.",
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
                description: "Initialize or rebuild the index for the current repository.",
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
        let status = if let Some(conn) = self.router.conn() {
            let file_count: i64 = conn
                .query_row("SELECT COUNT(*) FROM files", [], |row| row.get(0))
                .unwrap_or(0);
            let symbol_count: i64 = conn
                .query_row("SELECT COUNT(*) FROM symbols", [], |row| row.get(0))
                .unwrap_or(0);
            let ref_count: i64 = conn
                .query_row("SELECT COUNT(*) FROM \"references\"", [], |row| row.get(0))
                .unwrap_or(0);

            serde_json::json!({
                "indexed": true,
                "file_count": file_count,
                "symbol_count": symbol_count,
                "reference_count": ref_count
            })
        } else {
            serde_json::json!({
                "indexed": false
            })
        };

        format_result(&status, format)
    }

    fn tool_init(&self, args: Value) -> CallToolResult {
        let local = args.get("local").and_then(|v| v.as_bool()).unwrap_or(false);
        let format = extract_format(&args);

        match pipeline::build_index_with_progress(
            self.router.repo_root(),
            local,
            &Progress::silent(),
        ) {
            Ok(stats) => {
                let result = serde_json::json!({
                    "file_count": stats.file_count,
                    "symbol_count": stats.symbol_count,
                    "reference_count": stats.ref_count,
                    "elapsed_ms": stats.elapsed.as_millis()
                });
                format_result(&result, format)
            }
            Err(_) => CallToolResult::error("index build failed".into()),
        }
    }
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
        assert_eq!(tools.len(), 9);
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
    fn tools_list_returns_all_tools() {
        let server = test_server();
        let result = server.handle_tools_list();
        let tools = result["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 9);
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
}
