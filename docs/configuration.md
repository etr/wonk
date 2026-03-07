# Configuration

Configuration loads in layers (last wins):

1. Built-in defaults
2. Global config: `~/.wonk/config.toml`
3. Per-repo config: `<repo-root>/.wonk/config.toml`

Each layer only overrides the fields it sets. Absent fields keep their previous
value.

## Full example

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

## Sections

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
