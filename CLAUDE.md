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
| `indexer.rs` | Tree-sitter parsing — extracts symbols, references, and imports for 11 languages |
| `db.rs` | SQLite layer — schema (WAL mode), repo root detection, index path computation |
| `pipeline.rs` | Index build orchestration — parallel file walk + parse + batch insert; incremental re-indexing for daemon |
| `walker.rs` | File enumeration with gitignore/wonkignore support; worktree-aware boundary detection |
| `search.rs` | Text search wrapping the `grep` crate (ripgrep internals) |
| `ranker.rs` | Classifies results (Definition > CallSite > Import > Other > Comment > Test), deduplicates re-exports |
| `output.rs` | Dual format: grep-compatible (stdout+stderr) or NDJSON (stdout) |
| `daemon.rs` | Background file watcher — double-fork daemonization, PID file, idle timeout, SIGTERM handler |
| `watcher.rs` | Filesystem event classification and debouncing via `notify` |
| `config.rs` | Layered TOML config: built-in defaults → `~/.wonk/config.toml` → `<repo>/.wonk/config.toml` |
| `mcp.rs` | MCP server — JSON-RPC 2.0 over stdio, exposes 9 query tools for AI coding assistants |
| `budget.rs` | Token budget tracking (~4 chars/token heuristic) |

### Key Design Decisions

- **No async runtime** — sync Rust + rayon for CPU-bound tree-sitter parsing
- **No IPC** — CLI and daemon communicate only via shared SQLite (WAL mode + busy_timeout)
- **Single crate** — lib.rs exports all modules; main.rs is a thin entry point
- **Bundled everything** — SQLite, tree-sitter grammars, grep engine all compiled into a single static binary
- **Worktree isolation** — each git worktree gets a separate index at `~/.wonk/index/<hash>/`; walker skips nested `.git` boundaries

### Supported Languages (tree-sitter)

TypeScript, TSX, JavaScript, Python, Rust, Go, Java, C, C++, Ruby, PHP

## Specifications

Detailed product requirements and architecture docs live in `specs/`:
- `specs/product_specs.md` — PRD with EARS requirements
- `specs/architecture.md` — architectural decisions and component design
- `specs/tasks.md` — development task tracking
