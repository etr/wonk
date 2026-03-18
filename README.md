# Wonk

[![CI](https://github.com/etr/wonk/actions/workflows/ci.yml/badge.svg)](https://github.com/etr/wonk/actions/workflows/ci.yml)
[![Crates.io](https://img.shields.io/crates/v/wonk)](https://crates.io/crates/wonk)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

**Structure-aware code search that cuts LLM token burn by 60%.**

## Before / after

Searching for `Blueprint` in the Flask repo -- ripgrep returns 225 lines of unsorted noise (changelogs, docs, tests, definitions all mixed together). Wonk returns the same matches **ranked and deduplicated**: definitions first, usages next, comments and tests last, with re-exports collapsed into `(+N other locations)` annotations.

<table>
<tr><th>rg Blueprint (225 lines)</th><th>wonk search Blueprint (213 lines)</th></tr>
<tr>
<td>

```
./src/flask/app.py:1119:  ...Blueprint`
./src/flask/app.py:1427:  ...Blueprint...
./src/flask/blueprints.py:10:from ...
./src/flask/blueprints.py:11:from ...
./src/flask/blueprints.py:18:class Blueprint...
./src/flask/__init__.py:3:from ...
./src/flask/debughelpers.py:8:from ...
./src/flask/debughelpers.py:146:  ...Blueprint
  ... 217 more unsorted lines ...
```

</td>
<td>

```
-- definitions --
src/flask/blueprints.py:18:class Blueprint(...)
  (+13 other locations)
src/flask/sansio/blueprints.py:119:class Blueprint(...)
  (+13 other locations)
-- usages --
src/flask/debughelpers.py:146:  ...Blueprint
src/flask/sansio/app.py:374:  self.blueprints: ...
  ... sorted by relevance ...
-- tests --
tests/test_blueprints.py:9:  ...
```

</td>
</tr>
</table>

Same data, structured for an LLM context window -- definitions surface instantly instead of buried on line 67.

## The problem

LLM coding agents grep aggressively. A single query can stuff hundreds of noisy, unranked lines into the context window -- raw matches with no sense of what is a definition, what is a test, and what is a re-export. That is wasted tokens and wasted money.

## How it works

Wonk pre-indexes your codebase with Tree-sitter so it understands code structure: definitions vs. usages, symbol kinds, scopes, imports, and dependencies. When you search, results come back **ranked, deduplicated, and grouped by relevance** -- definitions first, tests last. The index stays fresh via a background file watcher, and a built-in MCP server exposes 23 tools for AI coding assistants.

## Features at a glance

**Search**
- Smart ranking: definitions first, tests last, re-exports deduplicated
- Semantic search via Ollama embeddings (`wonk ask`)
- Hybrid RRF fusion blends structural + semantic results (`--semantic`)

**Code intelligence**
- Symbol lookup, signatures, and full source display (`sym`, `sig`, `show`)
- Call graph traversal: callers, callees, shortest call path
- Blast radius analysis with severity tiers and risk levels
- Execution flow tracing from entry points
- Changed symbol detection with blast/flow chaining

**Architecture**
- Single static binary -- SQLite, tree-sitter grammars, and grep engine bundled
- 12 languages: TypeScript/TSX, JavaScript, Python, Rust, Go, Java, C, C++, Ruby, PHP, C#
- Background daemon keeps index fresh via filesystem watcher
- Worktree isolation -- separate index per git worktree
- 23 MCP tools for AI coding assistants (JSON-RPC 2.0 over stdio)
- Token budget (`--budget N`) caps output and preserves top-ranked results

## Benchmarks

25 code-understanding tasks across 5 real-world repos (ripgrep, tokio, httpx, pydantic, fastify), 5 runs each, median reported. Measures Claude Code token consumption with vs without wonk.

| Category | Baseline (avg) | Wonk (avg) | Reduction | Quality (B→W) |
|----------|---------------:|-----------:|----------:|--------------:|
| symbol_location | 100k | 61k | 33% | 0.85→0.85 |
| reference_tracing | 96k | 57k | 28% | 0.92→0.88 |
| architecture | 162k | 101k | 29% | 0.90→0.96 |
| multi_step | 143k | 104k | 23% | 0.93→0.93 |
| structural | 130k | 69k | 46% | 0.95→0.88 |

**Overall:** 37.4% total reduction (median per-task 29.7%, best 68.5%). Quality maintained at 0.90 vs 0.91 baseline.

## Installation

### curl (Linux / macOS)

```sh
curl -fsSL https://raw.githubusercontent.com/etr/wonk/main/install.sh | sh
```

### Cargo

```sh
cargo install wonk
```

### Building from source

```sh
git clone https://github.com/etr/wonk.git && cd wonk
cargo build --release
# Binary: target/release/wonk
```

## Quick start

```sh
cd your-project
wonk search "handleRequest"       # ranked full-text search
wonk sym "UserService"            # find symbol definitions
wonk callers "dispatch"           # who calls this?
wonk blast "processPayment"       # what breaks if this changes?
wonk changes --blast --flows      # changed symbols + impact analysis
wonk ask "error handling logic"   # semantic search (requires Ollama)
```

Indexing happens automatically on first use.

## Claude Code plugin

The [wonk plugin](https://github.com/etr/wonk-plugin) integrates wonk into [Claude Code](https://docs.anthropic.com/en/docs/claude-code) as a native tool provider. It bundles the MCP server, an agent skill that teaches Claude when to prefer wonk over grep/glob, and a session hook that keeps the index fresh.

```sh
# Recommended: install via Groundwork Marketplace
claude plugin marketplace add https://github.com/etr/groundwork-marketplace
claude plugin install wonk
```

See the [wonk-plugin repo](https://github.com/etr/wonk-plugin) for alternative installation methods.

## Commands

| Command | Description |
|---------|-------------|
| **Search** | |
| `search <pattern>` | Full-text search with smart ranking, dedup, `--semantic` fusion |
| `ask <query>` | Semantic search via embedding similarity |
| **Symbol lookup** | |
| `sym <name>` | Symbol definitions by name, kind, or exact match |
| `ref <name>` | Find references to a symbol |
| `sig <name>` | Show function/method signatures |
| `show <name>` | Show full source body (`--shallow` for containers) |
| **Code structure** | |
| `ls [path]` | List files and symbols (`--tree` for structure) |
| `deps <file>` | Show file dependencies (imports) |
| `rdeps <file>` | Show reverse dependencies |
| `summary <path>` | Structural summary with optional `--semantic` description |
| **Call graph** | |
| `callers <name>` | Find callers with transitive `--depth` expansion |
| `callees <name>` | Find callees with transitive `--depth` expansion |
| `callpath <from> <to>` | Shortest call chain between two symbols |
| **Program analysis** | |
| `flows [entry]` | Detect entry points and trace execution flows |
| `blast <symbol>` | Blast radius with severity tiers and risk levels |
| `changes` | Changed symbols with optional `--blast` / `--flows` chaining |
| `context <name>` | Full symbol context: callers, callees, flows, children |
| `impact <file>` | Symbol-level change impact analysis |
| **Semantic** | |
| `cluster <path>` | Cluster symbols by semantic similarity (K-Means) |
| **Index management** | |
| `init` | Build index (auto-runs on first query) |
| `update` | Rebuild index |
| `status` | Show index stats |
| `repos list\|clean` | Manage tracked repositories |
| **Daemon** | |
| `daemon start\|stop\|status\|list` | Manage background file watcher |
| **Integration** | |
| `mcp serve` | Start MCP server (JSON-RPC 2.0 over stdio) |

Full flag reference: [`docs/commands.md`](docs/commands.md)

## Comparison with alternatives

| | wonk | ripgrep | ctags/LSP |
|---|---|---|---|
| Structural ranking | Definitions first, tests last | No ranking | N/A |
| Deduplication | Re-export collapsing | None | N/A |
| Call graph | Callers, callees, callpath, blast radius | No | LSP only (running server) |
| Semantic search | Embedding similarity (Ollama) | No | No |
| Token budget | `--budget N` caps output | No | No |
| Setup | Single binary, auto-indexes | Single binary | Language server per language |
| MCP server | 23 tools built-in | No | Via adapter |
| Output | grep-compatible + JSON + TOON | grep + JSON | Protocol-specific |

## Output formats

**grep** (default) -- standard grep-compatible format, pipe-friendly:
```
src/main.rs:42:fn main() {}
```

**json** (`--format json`) -- NDJSON, one object per line:
```json
{"file":"src/main.rs","line":42,"col":1,"content":"fn main() {}"}
```

**toon** (`--format toon`) -- compact, indentation-based, minimal punctuation:
```
file: src/main.rs
line: 42
content: fn main() {}
```

## Supported languages

TypeScript (TSX), JavaScript (JSX), Python, Rust, Go, Java, C, C++, Ruby, PHP, C#

## Optional dependencies

Wonk's core features work out of the box with zero external dependencies. Advanced features require:

- **[Ollama](https://ollama.ai/)** -- for semantic search and AI-generated summaries. Pull `nomic-embed-text` (embeddings) and `llama3.2:3b` (summaries).
- **git** -- only needed for `wonk impact --since` and `wonk changes --scope compare`. Most likely already installed.

## MCP server

Wonk includes a built-in [MCP](https://modelcontextprotocol.io/) server for AI coding assistants. Add to your `.mcp.json`:

```json
{
  "mcpServers": {
    "wonk": {
      "command": "wonk",
      "args": ["mcp", "serve"]
    }
  }
}
```

23 tools exposed: search, sym, ref, sig, show, ls, deps, rdeps, callers, callees, callpath, summary, flows, blast, changes, context, ask, cluster, impact, init, update, status, repos. All tools accept an optional `repo` parameter for multi-repo setups.

## Configuration

Layered TOML config: built-in defaults < `~/.wonk/config.toml` < `<repo>/.wonk/config.toml`.

Full reference: [`docs/configuration.md`](docs/configuration.md)

## Acknowledgments

Built with [Claude Code](https://docs.anthropic.com/en/docs/claude-code) and [Groundwork](https://github.com/etr/groundwork).

## License

MIT
