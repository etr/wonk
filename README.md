# Wonk

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
WONK_VERSION=0.2.0 WONK_INSTALL=$HOME/.local/bin \
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

## Commands

### `wonk search <pattern>`

Full-text search across indexed files.

```
wonk search "handleRequest"
wonk search --regex "handle\w+Request"
wonk search -i "config"
wonk search "render" -- src/components/
```

| Flag | Description |
|------|-------------|
| `--regex` | Treat pattern as a regular expression |
| `-i`, `--ignore-case` | Case-insensitive search |
| `--raw` | Skip ranking, deduplication, and category headers |
| `--smart` | Force smart ranking even if pattern does not match known symbols |
| `-- <paths>` | Restrict search to specific paths |

### `wonk sym <name>`

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

### `wonk ref <name>`

Find references to a symbol across the codebase.

```
wonk ref "handleRequest"
wonk ref "validate" -- src/
```

| Flag | Description |
|------|-------------|
| `-- <paths>` | Restrict search to specific paths |

### `wonk sig <name>`

Show function and method signatures.

```
wonk sig "process"
```

Output:

```
src/engine.rs:15:  fn process(input: &str) -> Result<()>
```

### `wonk ls [path]`

List indexed files. Defaults to the repository root.

```
wonk ls
wonk ls src/components
wonk ls --tree
```

| Flag | Description |
|------|-------------|
| `--tree` | Show files with symbol structure (functions, classes, methods) |

### `wonk deps <file>`

Show files that a given file depends on (imports/requires).

```
wonk deps src/main.rs
```

Output:

```
src/main.rs -> src/lib.rs
src/main.rs -> src/config.rs
```

### `wonk rdeps <file>`

Show reverse dependencies -- files that depend on a given file.

```
wonk rdeps src/config.rs
```

### `wonk init`

Manually initialize indexing for the current repository. This is optional --
any query command (`search`, `sym`, `ref`, `sig`, `ls`, `deps`, `rdeps`)
automatically builds the index on first use. Use `init` when you want to
pre-build the index or choose `--local` mode.

```
wonk init
wonk init --local
```

| Flag | Description |
|------|-------------|
| `--local` | Use a project-specific index instead of the shared index |

### `wonk update`

Re-index the current repository.

```
wonk update
```

### `wonk status`

Show indexing status for the current repository.

```
wonk status
```

### `wonk daemon <start|stop|status>`

Manage the background daemon.

```
wonk daemon start
wonk daemon stop
wonk daemon status
```

### `wonk repos <list|clean>`

Manage tracked repositories.

```
wonk repos list
wonk repos clean    # Remove stale repositories from the index
```

### `wonk mcp serve`

Start an MCP (Model Context Protocol) server over stdio. This lets AI coding
assistants like Claude Code use wonk as a tool provider.

```
wonk mcp serve
```

The server communicates via JSON-RPC 2.0 over NDJSON on stdin/stdout. It
auto-indexes the repository on first startup if no index exists.

## Global flags

These flags work with any command:

| Flag | Description |
|------|-------------|
| `--json` | Output results as JSON (one object per line) |
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
idle_timeout_minutes = 30     # Minutes of inactivity before daemon shuts down
debounce_ms = 500             # Debounce interval for file-change events (ms)

[index]
max_file_size_kb = 1024       # Skip files larger than this (KiB)
additional_extensions = []    # Extra file extensions to index

[output]
default_format = "grep"       # "grep" or "json"
color = "auto"                # "auto", "always", or "never"

[ignore]
patterns = []                 # Glob patterns to exclude from indexing
```

### Sections

**`[daemon]`**

| Key | Default | Description |
|-----|---------|-------------|
| `idle_timeout_minutes` | `30` | Minutes of inactivity before the daemon shuts down |
| `debounce_ms` | `500` | Debounce interval in milliseconds for file-change events |

**`[index]`**

| Key | Default | Description |
|-----|---------|-------------|
| `max_file_size_kb` | `1024` | Maximum file size in KiB that the indexer will process |
| `additional_extensions` | `[]` | Extra file extensions to index beyond the built-in set |

**`[output]`**

| Key | Default | Description |
|-----|---------|-------------|
| `default_format` | `"grep"` | Default output format: `"grep"` or `"json"` |
| `color` | `"auto"` | Color mode: `"auto"`, `"always"`, or `"never"` |

**`[ignore]`**

| Key | Default | Description |
|-----|---------|-------------|
| `patterns` | `[]` | Glob patterns to exclude from walks and indexing |

## Background daemon

Wonk runs a background daemon that watches for file changes and keeps the index
up to date. The daemon:

- Auto-spawns on first query (including after auto-indexing) if not already running
- Debounces file-system events (default: 500ms)
- Shuts down after idle timeout (default: 30 minutes)
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

### JSON (`--json`)

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

## Integrating with LLM agents

Wonk's grep-compatible output means any tool that can call `grep` or `rg` can
call `wonk search` instead -- no integration work required. For programmatic
use:

- `--json` gives structured NDJSON output
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

The server exposes 9 tools over stdio (JSON-RPC 2.0):

| Tool | Description |
|------|-------------|
| `wonk_search` | Full-text search with structural ranking and optional token budget |
| `wonk_sym` | Look up symbol definitions by name, kind, or exact match |
| `wonk_ref` | Find references to a symbol |
| `wonk_sig` | Show function/method signatures |
| `wonk_ls` | List files and symbols in a path |
| `wonk_deps` | Show file dependencies (imports) |
| `wonk_rdeps` | Show reverse dependencies |
| `wonk_status` | Check index status (file/symbol/reference counts) |
| `wonk_init` | Initialize or rebuild the index |

All file paths are validated against the repository boundary. The index is
built automatically on first use if it does not already exist.

## License

MIT
