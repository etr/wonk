# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Build & Development Commands

```bash
cargo build                    # Build debug binary
cargo build --release          # Build release binary
cargo test                     # Run all tests
cargo test <test_name>         # Run a single test by name
cargo test --lib               # Run only unit tests (no integration tests)
cargo fmt --check              # Check formatting (CI enforced)
cargo fmt                      # Auto-format code
cargo clippy -- -D warnings    # Lint with warnings-as-errors (CI enforced)
```

CI enforces `RUSTFLAGS="-D warnings"` — all warnings are errors.

## Architecture

Wonk is a structure-aware code search CLI for LLM coding agents. It combines tree-sitter parsing, SQLite indexing, and ripgrep-based text search to return ranked, deduplicated results that minimize token consumption.

### Data Flow

```
CLI (clap) → Router → { SQLite index | grep search } → Ranker → Budget → Output
                                    ↑
                              Daemon (notify → pipeline → SQLite)
```

### Module Responsibilities

| Module | Role |
|--------|------|
| `cli.rs` | Clap-derived argument parsing, delegates to `router::dispatch()` |
| `router.rs` | Query dispatch — routes commands to index or grep fallback, auto-initializes index on first use |
| `indexer.rs` | Tree-sitter parsing — extracts symbols, references, and imports for 12 languages |
| `db.rs` | SQLite layer — schema (WAL mode), repo root detection, index path computation |
| `pipeline.rs` | Index build orchestration — parallel file walk + parse + batch insert; incremental re-indexing for daemon; embedding build pipeline (chunking → Ollama batch embed → vector storage) |
| `walker.rs` | File enumeration with gitignore/wonkignore support; worktree-aware boundary detection |
| `search.rs` | Text search wrapping the `grep` crate (ripgrep internals) |
| `semantic.rs` | Brute-force cosine similarity search — parallel dot product via rayon, top-N ranking, resolution of symbol IDs to SemanticResult structs; BFS dependency graph traversal for `--from`/`--to` reachability scoping |
| `cluster.rs` | K-Means clustering engine — auto-k selection via silhouette scoring, cluster member resolution from SQLite |
| `ranker.rs` | Classifies results (Definition > CallSite > Import > Other > Comment > Test), deduplicates re-exports; RRF fusion via `fuse_rrf()` for `--semantic` blending |
| `output.rs` | Dual format: grep-compatible (stdout+stderr) or NDJSON (stdout) |
| `embedding.rs` | Ollama API client, symbol chunking engine, and vector storage — sync HTTP client for embedding generation, context-rich text chunk formatting for `nomic-embed-text`, BLOB storage/retrieval with bytemuck zero-copy deserialization |
| `show.rs` | Source body retrieval — queries symbol index by name with optional file/kind/exact filters, reads source file lines line..end_line per match, falls back to signature when end_line is absent; shallow mode for containers shows signature + child signatures without bodies |
| `summary.rs` | Structural summary engine — queries SQLite to aggregate file count, line count, symbol counts by kind, language breakdown, and dependency count for a path; supports three detail levels (rich/light/symbols) and recursive depth traversal |
| `llm.rs` | LLM description generation and caching — content hash computation from (symbol.id, file.hash) pairs, prompt construction from structural metrics, Ollama `/api/generate` sync client, SQLite cache get/store for `wonk summary --semantic` |
| `impact.rs` | Symbol change detection — Tree-sitter re-parse vs. indexed symbols for Added/Modified/Removed; git CLI wrapper for `--since` file listing; scoped change detection via `ChangeScope` enum and `detect_changes()` with git diff hunk-to-symbol mapping |
| `daemon.rs` | Background file watcher — double-fork daemonization, PID file, SIGTERM handler, embedding worker thread, daemon status table |
| `watcher.rs` | Filesystem event classification and debouncing via `notify` |
| `config.rs` | Layered TOML config: built-in defaults → `~/.wonk/config.toml` → `<repo>/.wonk/config.toml` |
| `blast.rs` | Blast radius analysis — depth-annotated BFS from a target symbol, severity tiers (WILL BREAK / LIKELY AFFECTED / MAY NEED TESTING), risk level assessment, inheritance integration via type_edges, test file exclusion |
| `callgraph.rs` | Call graph traversal — BFS callers/callees expansion and callpath shortest-path finder via caller_id JOIN, cycle detection via visited HashSet, depth cap enforcement |
| `flows.rs` | Entry point detection and flow tracing — SQL anti-join for functions/methods with no indexed callers, forward BFS callee expansion with depth/branching/min-confidence controls |
| `mcp.rs` | MCP server — JSON-RPC 2.0 over stdio, exposes 17 query tools for AI coding assistants |
| `budget.rs` | Token budget tracking (~4 chars/token heuristic) |

### Key Design Decisions

- **No async runtime** — sync Rust + rayon for CPU-bound tree-sitter parsing
- **No IPC** — CLI and daemon communicate only via shared SQLite (WAL mode + busy_timeout)
- **Single crate** — lib.rs exports all modules; main.rs is a thin entry point
- **Bundled everything** — SQLite, tree-sitter grammars, grep engine all compiled into a single static binary
- **Worktree isolation** — each git worktree gets a separate index at `~/.wonk/index/<hash>/`; walker skips nested `.git` boundaries

### Supported Languages (tree-sitter)

TypeScript, TSX, JavaScript, Python, Rust, Go, Java, C, C++, Ruby, PHP, C#

## Specifications

Detailed product requirements and architecture docs live in `specs/`:
- `specs/product_specs.md` — PRD with EARS requirements
- `specs/architecture.md` — architectural decisions and component design
- `specs/tasks.md` — development task tracking
