# Wonk

> **Disclaimer:** This tool was vibe-coded with love and little to no human supervision
> using [Claude Code](https://docs.anthropic.com/en/docs/claude-code) and
> [Groundwork](https://github.com/etr/groundwork).

Structure-aware code search that cuts LLM token burn.

## The problem

LLM coding agents grep aggressively. A single query can stuff hundreds of
noisy, unranked lines into the context window -- raw matches with no sense of
what is a definition, what is a test, and what is a re-export. That is wasted
tokens and wasted money.

## How it works

Wonk pre-indexes your codebase with Tree-sitter so it understands code
structure: definitions vs. usages, symbol kinds, scopes, imports, and
dependencies. When you search, results come back **ranked, deduplicated, and
grouped by relevance** -- definitions first, tests last. The index stays fresh
via a background file watcher, and output is grep-compatible so existing tools
work with zero integration.

## Quick start

```sh
# Install
curl -fsSL https://raw.githubusercontent.com/etr/wonk/main/install.sh | sh

# Search -- indexing happens automatically on first use
cd your-project
wonk search "handleRequest"
```

## Installation

### curl (Linux / macOS)

```sh
curl -fsSL https://raw.githubusercontent.com/etr/wonk/main/install.sh | sh
```

Environment variables:

| Variable | Default | Description |
|----------|---------|-------------|
| `WONK_VERSION` | latest | Version to install |
| `WONK_INSTALL` | `/usr/local/bin` | Installation directory |

Example:

```sh
WONK_VERSION=2.0.0 WONK_INSTALL=$HOME/.local/bin \
  curl -fsSL https://raw.githubusercontent.com/etr/wonk/main/install.sh | sh
```

### Cargo

```sh
cargo install wonk
```

### Building from source

```sh
git clone https://github.com/etr/wonk.git
cd wonk
cargo build --release
# Binary is at target/release/wonk
```

## Optional dependencies

Wonk's core features -- structural search, symbol lookup, call graph analysis,
and change detection -- work out of the box with zero external dependencies.
Advanced semantic and LLM features require Ollama; git-based change detection
requires git.

### Ollama

[Ollama](https://ollama.ai/) provides local embedding generation and LLM text
generation. Install it and pull the two models wonk uses:

```sh
# Install Ollama (see https://ollama.ai for other methods)
curl -fsSL https://ollama.ai/install.sh | sh

# Pull the embedding model (required for semantic search)
ollama pull nomic-embed-text

# Pull the text generation model (required for AI-generated summaries)
ollama pull llama3.2:3b
```

| Model | Used by |
|-------|---------|
| `nomic-embed-text` | `wonk ask`, `wonk search --semantic`, `wonk cluster`, `wonk init` (embedding build) |
| `llama3.2:3b` | `wonk summary --semantic` |

Embeddings are built automatically on first semantic query or explicitly via
`wonk init`. The background daemon keeps them up to date as files change.

To point at a remote Ollama instance or swap models, add to your
`~/.wonk/config.toml` (or `<repo>/.wonk/config.toml`):

```toml
[llm]
model = "llama3.2:3b"
generate_url = "http://your-server:11434/api/generate"
```

The embedding client uses `http://localhost:11434` by default. No separate
configuration is needed when Ollama runs locally.

### Git

Git is only required for commit-relative change detection:

- `wonk impact --since <commit>`
- `wonk changes --scope compare --base <ref>`

Git is most likely already installed on your system. If not, see
[git-scm.com](https://git-scm.com/).

## Commands

### Search

#### `wonk search <pattern>`

Full-text search across indexed files.

```
wonk search "handleRequest"
wonk search --regex "handle\w+Request"
wonk search -i "config"
wonk search --semantic "render"
wonk search "render" -- src/components/
```

| Flag | Description |
|------|-------------|
| `--regex` | Treat pattern as a regular expression |
| `-i`, `--ignore-case` | Case-insensitive search |
| `--raw` | Skip ranking, deduplication, and category headers |
| `--smart` | Force smart ranking even if pattern does not match known symbols |
| `--semantic` | Blend structural results with embedding-based semantic results (RRF fusion) |
| `-- <paths>` | Restrict search to specific paths |

#### `wonk ask <query>`

Semantic search: find symbols related to a natural language query.
Requires Ollama running locally with `nomic-embed-text`.

```
wonk ask "error handling logic"
wonk ask --from src/api.rs "authentication"
wonk ask --to src/db.rs "query builder"
```

| Flag | Description |
|------|-------------|
| `--from <file>` | Restrict to symbols reachable from this file |
| `--to <file>` | Restrict to symbols that can reach this file |

### Symbol lookup

#### `wonk sym <name>`

Look up symbol definitions (functions, classes, variables, etc.).

```
wonk sym "UserService"
wonk sym --kind function "process"
wonk sym --exact "Config"
```

| Flag | Description |
|------|-------------|
| `--kind <kind>` | Filter by symbol kind (e.g. `function`, `class`, `variable`) |
| `--exact` | Require exact match on symbol name |

#### `wonk ref <name>`

Find references to a symbol across the codebase.

```
wonk ref "handleRequest"
wonk ref "validate" -- src/
```

| Flag | Description |
|------|-------------|
| `-- <paths>` | Restrict search to specific paths |

#### `wonk sig <name>`

Show function and method signatures.

```
wonk sig "process"
```

Output:

```
src/engine.rs:15:  fn process(input: &str) -> Result<()>
```

#### `wonk show <name>`

Show the full source body of a symbol. For container types (class, struct,
enum, trait, interface), use `--shallow` to get the container signature plus
child signatures without bodies.

```
wonk show "processPayment"
wonk show --file src/billing.ts "processPayment"
wonk show --kind function "handle"
wonk show --shallow "MyClass"
```

| Flag | Description |
|------|-------------|
| `--file <path>` | Restrict results to a specific file |
| `--kind <kind>` | Filter by symbol kind (e.g. `function`, `class`) |
| `--exact` | Require exact match on symbol name |
| `--shallow` | Show container signature + child signatures without bodies |

### Code structure

#### `wonk ls [path]`

List indexed files. Defaults to the repository root.

```
wonk ls
wonk ls src/components
wonk ls --tree
```

| Flag | Description |
|------|-------------|
| `--tree` | Show files with symbol structure (functions, classes, methods) |

#### `wonk deps <file>`

Show files that a given file depends on (imports/requires).

```
wonk deps src/main.rs
```

Output:

```
src/main.rs -> src/lib.rs
src/main.rs -> src/config.rs
```

#### `wonk rdeps <file>`

Show reverse dependencies -- files that depend on a given file.

```
wonk rdeps src/config.rs
```

#### `wonk summary <path>`

Show a structural summary of a file or directory: file count, line count,
symbol counts by kind, language breakdown, and dependency count.

```
wonk summary src/
wonk summary --detail light src/auth/
wonk summary --recursive src/
wonk summary --semantic src/lib.rs
```

| Flag | Description |
|------|-------------|
| `--detail <level>` | Detail level: `rich` (default), `light`, or `symbols` |
| `--depth <N>` | Recursion depth for child summaries (0 = target only) |
| `--recursive` | Show full recursive hierarchy (unlimited depth) |
| `--semantic` | Include AI-generated description (requires Ollama) |

### Call graph

Wonk tracks caller/callee relationships by analyzing which symbols appear
within other symbols' bodies. This call graph powers the `callers`, `callees`,
`callpath`, `flows`, `blast`, `changes`, and `context` commands.

#### `wonk callers <name>`

Find all callers of a symbol (functions whose bodies reference it).

```
wonk callers "dispatch"
wonk callers --depth 3 "dispatch"
wonk callers --min-confidence 0.8 "dispatch"
```

| Flag | Description |
|------|-------------|
| `--depth <N>` | Transitive expansion depth (default: 1 = direct callers only, max: 10) |
| `--min-confidence <F>` | Minimum edge confidence threshold (0.0-1.0) |

#### `wonk callees <name>`

Find all callees of a symbol (symbols referenced within its body).

```
wonk callees "main"
wonk callees --depth 2 "main"
```

| Flag | Description |
|------|-------------|
| `--depth <N>` | Transitive expansion depth (default: 1 = direct callees only, max: 10) |
| `--min-confidence <F>` | Minimum edge confidence threshold (0.0-1.0) |

#### `wonk callpath <from> <to>`

Find the shortest call chain between two symbols via BFS traversal.

```
wonk callpath "main" "dispatch"
wonk callpath --min-confidence 0.7 "handleRequest" "writeDB"
```

| Flag | Description |
|------|-------------|
| `--min-confidence <F>` | Minimum edge confidence threshold (0.0-1.0) |

### Program analysis

#### `wonk flows [entry]`

Detect entry points (functions/methods with no callers) and trace execution
flows via BFS callee expansion. Without an entry parameter, lists all detected
entry points. With an entry parameter, traces the full execution flow from that
function.

```
wonk flows                      # list all entry points
wonk flows "main"               # trace flow from main
wonk flows --from src/api.ts    # entry points in a specific file
wonk flows --depth 5 --branching 2 "handleRequest"
```

| Flag | Description |
|------|-------------|
| `--from <file>` | Restrict entry point detection to symbols in this file |
| `--depth <N>` | Maximum BFS traversal depth (default: 10, max: 20) |
| `--branching <N>` | Maximum callees to follow per symbol (default: 4) |
| `--min-confidence <F>` | Minimum edge confidence threshold (0.0-1.0) |

#### `wonk blast <symbol>`

Analyze the blast radius of a symbol change. Shows all affected symbols grouped
by severity tier (WILL BREAK, LIKELY AFFECTED, MAY NEED TESTING) with a risk
level assessment. Integrates inheritance edges (extends/implements).

```
wonk blast "processPayment"
wonk blast --direction downstream "validateInput"
wonk blast --depth 5 --include-tests "UserService"
```

| Flag | Description |
|------|-------------|
| `--direction <dir>` | Traversal direction: `upstream` (default) or `downstream` |
| `--depth <N>` | Maximum traversal depth (default: 3, max: 10) |
| `--include-tests` | Include test files in results |
| `--min-confidence <F>` | Minimum edge confidence threshold (0.0-1.0) |

#### `wonk changes`

Detect changed symbols in the working tree. Optionally chain blast radius
analysis and execution flow detection for each changed symbol.

```
wonk changes                              # unstaged changes
wonk changes --scope staged               # staged changes
wonk changes --scope all                  # all uncommitted changes
wonk changes --scope compare --base main  # compare to a ref
wonk changes --blast --flows              # chain blast + flow analysis
```

| Flag | Description |
|------|-------------|
| `--scope <scope>` | Change scope: `unstaged` (default), `staged`, `all`, or `compare` |
| `--base <ref>` | Base git ref for compare scope |
| `--blast` | Include blast radius analysis for each changed symbol |
| `--flows` | Identify execution flows affected by changed symbols |
| `--min-confidence <F>` | Minimum edge confidence for blast/flow edges (0.0-1.0) |

#### `wonk context <name>`

Aggregate full context for a symbol: definition, categorized incoming
references (callers, importers, type users), outgoing references (callees,
imports), flow participation, and children (extending/implementing types).

```
wonk context "processPayment"
wonk context --file src/billing.ts "processPayment"
wonk context --kind class "StripeClient"
```

| Flag | Description |
|------|-------------|
| `--file <path>` | Restrict to symbols in this file |
| `--kind <kind>` | Filter by symbol kind (e.g. `function`, `class`) |
| `--min-confidence <F>` | Minimum edge confidence threshold (0.0-1.0) |

### Change impact

#### `wonk impact <file>`

Analyze symbol changes and find semantically impacted downstream code.

```
wonk impact src/lib.rs
wonk impact --since HEAD~5
```

| Flag | Description |
|------|-------------|
| `--since <commit>` | Analyze all files changed since this commit |

### Semantic

#### `wonk cluster <path>`

Cluster symbols by semantic similarity within a directory.
Uses K-Means with automatic K selection via silhouette scoring.

```
wonk cluster src/
wonk cluster --top 3 src/components/
```

| Flag | Description |
|------|-------------|
| `--top <N>` | Representative symbols per cluster (default: 5) |

### Index management

#### `wonk init`

Manually initialize indexing for the current repository. This is optional --
any query command automatically builds the index on first use.

```
wonk init
wonk init --local
```

| Flag | Description |
|------|-------------|
| `--local` | Use a project-specific index instead of the shared index |

#### `wonk update`

Re-index the current repository.

```
wonk update
```

#### `wonk status`

Show indexing status for the current repository.

```
wonk status
```

#### `wonk repos <list|clean>`

Manage tracked repositories.

```
wonk repos list
wonk repos clean    # Remove stale repositories from the index
```

### Daemon

#### `wonk daemon <start|stop|status|list>`

Manage the background daemon.

```
wonk daemon start
wonk daemon stop
wonk daemon stop --all
wonk daemon status
wonk daemon list
```

| Flag | Description |
|------|-------------|
| `--all` | Stop all running daemons (with `stop`) |

### Integration

#### `wonk mcp serve`

Start an MCP (Model Context Protocol) server over stdio. This lets AI coding
assistants like Claude Code use wonk as a tool provider.

```
wonk mcp serve
```

## Global flags

These flags work with any command:

| Flag | Description |
|------|-------------|
| `--format <format>` | Output format: `grep` (default), `json`, or `toon` |
| `-q`, `--quiet` | Suppress hint messages on stderr |
| `--budget <N>` | Limit output to approximately N tokens (higher-ranked results preserved) |

## Smart search

When `wonk search` detects that your pattern matches known symbols in the
index, it automatically activates smart mode. Results are classified into
categories and sorted by relevance tier:

| Tier | Category | Description |
|------|----------|-------------|
| 0 | Definition | Symbol definitions (functions, classes, etc.) |
| 1 | CallSite | Call sites and usage references |
| 2 | Import | Import/require/use statements |
| 3 | Other | Unclassified matches |
| 4 | Comment | Comment-only lines |
| 5 | Test | Matches in test files |

Results are grouped under section headers on stderr:

```
-- definitions --
src/lib.rs:10:pub fn foo() {}  (+2 other locations)
-- usages --
src/main.rs:25:    foo();
src/handler.rs:42:    let result = foo();
-- comments --
src/lib.rs:8:// foo handles the primary workflow
-- tests --
tests/test_foo.rs:15:    assert!(foo().is_ok());
```

Re-exported symbols are deduplicated: when a definition exists, import
re-exports are collapsed into the definition's annotation
`(+N other locations)`. When no definition exists, imports appear under their
own `-- imports --` header.

Use `--raw` to disable all ranking, deduplication, and headers. Use `--smart`
to force smart mode even when the pattern does not match known symbols.

## Semantic search

Wonk supports embedding-based semantic search via [Ollama](https://ollama.ai/)
with the `nomic-embed-text` model. This lets you search by meaning rather than
exact text patterns.

- **Setup**: Install Ollama and pull `nomic-embed-text` (`ollama pull nomic-embed-text`)
- **Embedding build**: Embeddings are built on first semantic query or explicitly via `wonk init`
- **Freshness**: The background daemon keeps embeddings up to date as files change
- **Dependency scoping**: Use `--from <file>` and `--to <file>` to restrict
  semantic results to symbols reachable from or leading to a specific file,
  using the indexed dependency graph
- **Hybrid fusion**: `wonk search --semantic` blends structural and semantic
  result lists using Reciprocal Rank Fusion (RRF). The fusion constant K
  is configurable via `[search] rrf_k` (default: 60.0); higher values produce
  more even blending

Use `wonk ask` for pure semantic search, or `wonk search --semantic` to blend
structural and semantic results.

## Edge confidence

Wonk assigns a confidence score to each caller/callee edge based on how the
relationship was resolved:

| Confidence | Resolution method |
|------------|-------------------|
| >= 0.9 | Import-resolved: the callee was imported in the caller's file |
| >= 0.8 | Same-file: both symbols are defined in the same file |
| <= 0.5 | Fuzzy: name matched but no import or co-location evidence |

Use `--min-confidence <F>` on any graph command (`callers`, `callees`,
`callpath`, `flows`, `blast`, `changes`, `context`) to filter out low-confidence
edges. For example, `--min-confidence 0.8` keeps only import-resolved and
same-file edges.

## Supported languages

Wonk ships with Tree-sitter grammars for:

- TypeScript (including TSX)
- JavaScript (including JSX)
- Python
- Rust
- Go
- Java
- C
- C++
- Ruby
- PHP
- C#

## Configuration

Configuration loads in layers (last wins):

1. Built-in defaults
2. Global config: `~/.wonk/config.toml`
3. Per-repo config: `<repo-root>/.wonk/config.toml`

Each layer only overrides the fields it sets. Absent fields keep their previous
value.

### Full example

```toml
[daemon]
debounce_ms = 500             # Debounce interval for file-change events (ms)

[index]
max_file_size_kb = 1024       # Skip files larger than this (KiB)
additional_extensions = []    # Extra file extensions to index

[output]
default_format = "grep"       # "grep", "json", or "toon"
color = "auto"                # "auto", "always", or "never"

[ignore]
patterns = []                 # Glob patterns to exclude from indexing

[llm]
model = "llama3.2:3b"                              # Ollama model for text generation
generate_url = "http://localhost:11434/api/generate" # Ollama generate endpoint

[search]
rrf_k = 60.0                  # Reciprocal Rank Fusion constant K
```

### Sections

**`[daemon]`**

| Key | Default | Description |
|-----|---------|-------------|
| `debounce_ms` | `500` | Debounce interval in milliseconds for file-change events |

**`[index]`**

| Key | Default | Description |
|-----|---------|-------------|
| `max_file_size_kb` | `1024` | Maximum file size in KiB that the indexer will process |
| `additional_extensions` | `[]` | Extra file extensions to index beyond the built-in set |

**`[output]`**

| Key | Default | Description |
|-----|---------|-------------|
| `default_format` | `"grep"` | Default output format: `"grep"`, `"json"`, or `"toon"` |
| `color` | `"auto"` | Color mode: `"auto"`, `"always"`, or `"never"` |

**`[ignore]`**

| Key | Default | Description |
|-----|---------|-------------|
| `patterns` | `[]` | Glob patterns to exclude from walks and indexing |

**`[llm]`**

| Key | Default | Description |
|-----|---------|-------------|
| `model` | `"llama3.2:3b"` | Ollama model name for text generation (`wonk summary --semantic`) |
| `generate_url` | `"http://localhost:11434/api/generate"` | Full URL for the Ollama generate endpoint |

**`[search]`**

| Key | Default | Description |
|-----|---------|-------------|
| `rrf_k` | `60.0` | Reciprocal Rank Fusion constant K for `--semantic` blending |

## Background daemon

Wonk runs a background daemon that watches for file changes and keeps the index
up to date. The daemon:

- Auto-spawns on first query (including after auto-indexing) if not already running
- Debounces file-system events (default: 500ms)
- Runs indefinitely until explicitly stopped
- Manages its PID file automatically

Use `wonk daemon start`, `wonk daemon stop`, and `wonk daemon status` to
manage it directly.

## Git worktree support

Wonk detects git worktree boundaries and maintains a separate index and daemon
per worktree. Each worktree gets its own isolated index so concurrent work on
different branches does not interfere.

## Output formats

### Grep (default)

Standard grep-compatible format on stdout. Category headers and hints go to
stderr so they don't break pipe chains.

```
src/main.rs:42:fn main() {}
src/lib.rs:10:pub fn foo() {}
```

### JSON (`--format json`)

One JSON object per line (NDJSON) on stdout. Hints and headers are suppressed.

```json
{"file":"src/main.rs","line":42,"col":1,"content":"fn main() {}"}
{"file":"src/lib.rs","line":10,"col":1,"content":"pub fn foo() {}"}
```

When `--budget` truncates output in JSON mode, a final metadata line is
emitted:

```json
{"truncated_count":15,"budget_tokens":500,"used_tokens":498}
```

### Toon (`--format toon`)

[TOON](https://docs.rs/serde_toon2/) (Token-Oriented Object Notation) -- a
line-oriented, indentation-based format with minimal punctuation. Encodes the
same data as JSON but is more compact and easier to scan.

```
file: src/main.rs
line: 42
col: 1
content: fn main() {}

file: src/lib.rs
line: 10
col: 1
content: pub fn foo() {}
```

## Integrating with LLM agents

Wonk's grep-compatible output means any tool that can call `grep` or `rg` can
call `wonk search` instead -- no integration work required. For programmatic
use:

- `--format json` gives structured NDJSON output (or `--format toon` for a
  compact human-readable view)
- `--budget <N>` caps output to roughly N tokens, keeping the highest-ranked
  results and dropping noise
- `-q` suppresses stderr hints for clean machine parsing

### MCP server

Wonk includes a built-in [MCP](https://modelcontextprotocol.io/) server so AI
coding assistants can use it as a tool provider. To configure it in your
`.mcp.json`:

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

The server exposes 23 tools over stdio (JSON-RPC 2.0):

| Tool | Description |
|------|-------------|
| `wonk_search` | Full-text search with structural ranking and optional token budget |
| `wonk_sym` | Look up symbol definitions by name, kind, or exact match |
| `wonk_ref` | Find references to a symbol |
| `wonk_sig` | Show function/method signatures |
| `wonk_show` | Show full source body of a symbol (with shallow mode for containers) |
| `wonk_ls` | List files and symbols in a path |
| `wonk_deps` | Show file dependencies (imports) |
| `wonk_rdeps` | Show reverse dependencies |
| `wonk_callers` | Find callers of a symbol with transitive expansion |
| `wonk_callees` | Find callees of a symbol with transitive expansion |
| `wonk_callpath` | Find shortest call chain between two symbols |
| `wonk_summary` | Structural/semantic summary of a file or directory |
| `wonk_flows` | Detect entry points and trace execution flows |
| `wonk_blast` | Blast radius analysis with severity tiers and risk levels |
| `wonk_changes` | Detect changed symbols with optional blast/flow chaining |
| `wonk_context` | Aggregate full context for a symbol (definition + callers + callees + flows) |
| `wonk_ask` | Pure semantic search via embedding similarity |
| `wonk_cluster` | Cluster symbols by semantic similarity (K-Means) |
| `wonk_impact` | Analyze semantic impact of changed symbols |
| `wonk_init` | Initialize or rebuild the index (supports Ollama embedding build) |
| `wonk_update` | Rebuild the index for the current repository |
| `wonk_status` | Check index status (file/symbol/reference/embedding counts) |
| `wonk_repos` | List all indexed repositories (multi-repo mode only) |

All file paths are validated against the repository boundary. The index is
built automatically on first use if it does not already exist.

**Multi-repo support:** When multiple repositories are indexed, all tools
accept an optional `repo` parameter to target a specific repository. Use
`wonk_repos` to list available repositories.

### Claude Code plugin

The [wonk plugin](https://github.com/etr/wonk-plugin) integrates wonk into
[Claude Code](https://docs.anthropic.com/en/docs/claude-code) as a native
tool provider. It bundles the MCP server, an agent skill that teaches Claude
when to prefer wonk over grep/glob, and a session hook that keeps the index
fresh.

Install via the [Groundwork Marketplace](https://github.com/etr/groundwork-marketplace):

```sh
claude plugin marketplace add https://github.com/etr/groundwork-marketplace
claude plugin add wonk
```

Or directly from GitHub:

```sh
claude plugin add https://github.com/etr/wonk-plugin
```

## License

MIT
