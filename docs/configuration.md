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
bm25_k1 = 1.2                # BM25 term-frequency saturation strength
bm25_b = 0.75                # BM25 length-normalization strength

[embedding]
provider = "bundled"          # Offline default; use "ollama" for the opt-in tier

[review]                       # Diff-scoped review rule switches (wonk review)
breaking_change = true        # Family A: removed/signature-changed with callers
coverage_gap = true           # Family B: no test in the blast radius
cross_repo = true             # Family C: contract consumed by another indexed repo

[contracts]                   # Declared in <repo>/.wonk/config.toml ONLY
workspace = ["payments"]      # Workspace ids this repo links contracts across
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
| `bm25_k1` | `1.2` | BM25 term-frequency saturation strength (k1) for `--semantic` lexical re-ranking |
| `bm25_b` | `0.75` | BM25 length-normalization strength (b) in `[0, 1]`; `0` disables it |

**`[rank]`**

Signal-pipeline re-ranking for `wonk search` (smart ranked mode).

| Key | Default | Description |
|-----|---------|-------------|
| `enabled` | `false` | Route smart-ranked search results through the signal pipeline. `false` keeps the legacy ordering byte-for-byte |
| `weights.kind` | `1.0` | Weight of the kind signal (category tier ordering). A weight of `0` skips the signal entirely; absent names weigh zero |
| `weights.lexical` | `0.0` | BM25 score of the candidate's file over the query terms, min-max normalized across the candidate set (files without term statistics score 0). Requires a V5+ index with `term_stats` |
| `weights.semantic` | `0.0` | Cosine similarity between the query embedding and the candidate symbol's indexed embedding, mapped absolutely as `clamp01((cos + 1) / 2)`. Missing embeddings contribute zero, never a penalty. Requires indexed embeddings and the configured embedding provider (`[embedding] provider`, default bundled) |
| `weights.centrality` | `0.0` | `ln(1 + callers) / ln(1 + set_max)` over the symbol's distinct indexed callers, log-damped against the candidate set so a single hub cannot dominate unrelated queries |
| `weights.prominence` | `0.0` | `1.0` when the candidate defines a symbol named by the query (term or raw pattern), `0.5` when a query term appears as a whole identifier in the matched line, `0.0` for substring-only mentions |
| `weights.path_character` | `0.0` | Graded ladder value of the candidate's path: ordinary `1.0`, module entry `0.80`, barrel `0.70`, example `0.60`, shim `0.45`, type declaration `0.30`, test `0.20`, generated-shadowing-a-verified-peer `0.10`. Graded, never exclusion — a test file that is the best answer still ranks. A generated file is demoted only when a same-named hand-written peer exists in the index |
| `weights.proximity` | `0.0` | `1 / gap` over the first-occurrence positions of the query terms present as whole identifiers in the matched line (adjacent terms `1.0`, one token between `0.5`, decaying). Fewer than two present terms contribute zero |
| `weights.signature` | `0.0` | Answers signature-shaped queries (containing `(`, `->`, or `::`): `1.0` for the index-backed definition, `0.5` for a definition-shaped line (a parenthesis plus a definition keyword among its first three identifiers), `0.0` otherwise. Name-shaped queries are inert |

```toml
[rank]
enabled = true

[rank.weights]
kind = 1.0
```

Unknown signal names in `[rank.weights]` are a hard configuration error
naming the offender and the valid names, not a silent no-op. The weights
table replaces the previous layer's wholesale (per-repo over global over
default). `wonk search --why` opts into the pipeline for a single
invocation regardless of `enabled`, printing the per-signal breakdown.

**`[embedding]`**

| Key | Default | Description |
|-----|---------|-------------|
| `provider` | `"bundled"` | Embedding provider: offline bundled model, or opt-in `"ollama"` using `nomic-embed-text` |

Provider selection follows the normal configuration precedence: per-repo
configuration overrides global configuration, which overrides the built-in
default. `wonk init --provider <provider>` and
`wonk update --provider <provider>` override configuration for that invocation.

Embeddings from different providers or dimensions are kept in separate vector
spaces. If the active provider does not match the stored vectors, rebuild them:

```sh
wonk update --force --provider bundled
```

Use `--provider ollama` in that command when switching to the Ollama tier.

Ollama is a quality tier, not a requirement. When the configured Ollama
provider is unreachable at query time, semantic commands (`ask`,
`search --semantic`, `cluster`, `impact`) fall back to the bundled provider
and print a `warning:` on stderr. The fallback applies only while the stored
vectors are compatible (bundled, or none yet): a stored vector space that
disagrees with the resolved provider blocks the query with the re-embed
command above instead of silently searching — or overwriting — the wrong
space. `wonk status` always shows the active provider, the stored provider,
and the stored dimension.

**`[review]`**

| Key | Default | Description |
|-----|---------|-------------|
| `breaking_change` | `true` | Rule family A: a removed or signature-changed symbol with live indexed callers blocks |
| `coverage_gap` | `true` | Rule family B: an added/modified non-test symbol with no test in its blast radius warns |
| `cross_repo` | `true` | Rule family C: an added/modified/removed symbol providing a contract consumed by another indexed repo warns |

Each family layers independently — a noisy rule can be silenced alone without
touching the others. `cross_repo` needs both repos indexed into the central
registry with intersecting `[contracts] workspace` declarations; when the
inputs are unavailable the other families still run and review degrades with
a warning.

**`[contracts]`**

| Key | Default | Description |
|-----|---------|-------------|
| `http` | `true` | HTTP route/outbound-call detection |
| `env` | `true` | Environment-variable read/write detection |
| `queue` | `true` | Message-queue producer/consumer detection |
| `websocket` | `true` | WebSocket emit/handler-registration detection |
| `job` | `true` | Scheduled and background job detection |
| `grpc` | `true` | gRPC IDL + generated-stub detection |
| `graphql` | `true` | GraphQL resolver + operation detection |
| `openapi` | `true` | OpenAPI specification-document detection |
| `workspace` | `[]` | Workspace ids linking contracts across repos (string or array) |

`workspace` accepts a single string or an array of strings, and is honored only
in the per-repo `<repo-root>/.wonk/config.toml`. A value in the global config is
ignored with a warning on every command — grouping another developer's unrelated
repos is never someone else's global decision. Identifiers compare trimmed and
case-folded (`"Payments"` = `"payments"`). A repo that declares nothing defaults
to its own name as the workspace, so its consumers surface as `unscoped` rather
than `orphan` — a configuration gap is never labeled a defect. Declared
workspaces are written to the repo's `meta.json` at index time; sibling repos
are matched through that stored value, never through their working-tree config.

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
