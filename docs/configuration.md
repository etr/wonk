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

[feedback]                    # Usage-feedback capture (default off)
enabled = false               # Kill switch: no slates recorded, no feedback accepted
slate_retention = 64          # Most recent slates kept per repo
author_features = true        # Record author-derived slate features (last-touched-by, primary author)

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

Signal-pipeline re-ranking for `wonk search` (smart ranked mode). The
default weights below are the TASK-095 tuned table, flipped on as the
default after beating the legacy ordering on the labeled query set —
mean precision@10 0.5025 → 0.5175 with no query class regressing, and a
measured warm-query latency well under the 20 ms budget. The measurement
records live in `bench/rank-tuning-results.md` and
`bench/rank-latency-results.md`. Setting `enabled = false` keeps the
previous (legacy) ordering byte-for-byte — the escape hatch.

| Key | Default | Description |
|-----|---------|-------------|
| `enabled` | `true` | Route smart-ranked search results through the signal pipeline. `false` keeps the legacy ordering byte-for-byte |
| `weights.kind` | `1.0` | Weight of the kind signal (category tier ordering). A weight of `0` skips the signal entirely; absent names weigh zero |
| `weights.lexical` | `0.4` | BM25 score of the candidate's file over the query terms, min-max normalized across the candidate set (files without term statistics score 0). Requires a V5+ index with `term_stats` |
| `weights.semantic` | `0.3` | Cosine similarity between the query embedding and the candidate symbol's indexed embedding, mapped absolutely as `clamp01((cos + 1) / 2)`. Missing embeddings contribute zero, never a penalty. Requires indexed embeddings and the configured embedding provider (`[embedding] provider`, default bundled) |
| `weights.centrality` | `0.4` | `ln(1 + callers) / ln(1 + set_max)` over the symbol's distinct indexed callers, log-damped against the candidate set so a single hub cannot dominate unrelated queries |
| `weights.prominence` | `1.0` | `1.0` when the candidate defines a symbol named by the query (term or raw pattern), `0.5` when a query term appears as a whole identifier in the matched line, `0.0` for substring-only mentions |
| `weights.path_character` | `0.6` | Graded ladder value of the candidate's path: ordinary `1.0`, module entry `0.80`, barrel `0.70`, example `0.60`, shim `0.45`, type declaration `0.30`, test `0.20`, generated-shadowing-a-verified-peer `0.10`. Graded, never exclusion — a test file that is the best answer still ranks. A generated file is demoted only when a same-named hand-written peer exists in the index |
| `weights.proximity` | `0.0` | `1 / gap` over the first-occurrence positions of the query terms present as whole identifiers in the matched line (adjacent terms `1.0`, one token between `0.5`, decaying). Fewer than two present terms contribute zero |
| `weights.novelty` | `0.0` | Near-duplicate demotion (TASK-100): `1 - clamp01((jaccard - threshold) / (1 - threshold))` against the best sketch-Jaccard versus higher-ranked results carrying a signature, with `threshold` from `[duplicate]`. A weight of `0` (the default, pending bench evidence) skips the pass entirely; the earliest member of a duplicate group is never demoted, so every group keeps a representative |
| `weights.feedback` | `0.35` | The feedback-learned descriptive channel (TASK-102): the candidate's value is the clamped sum of the learned weights of the descriptive keys it carries (`path:src/auth`, `symbol:kind=trait`, …), zero without learned rows — the default weight is inert until feedback evidence accumulates and clears the `[feedback]` gates. `0` skips the pass entirely: the one-knob full disable |
| `weights.signature` | `0.8` | Answers signature-shaped queries (containing `(`, `->`, or `::`): `1.0` for the index-backed definition, `0.5` for a definition-shaped line (a parenthesis plus a definition keyword among its first three identifiers), `0.0` otherwise. Name-shaped queries are inert |
| `class_multipliers.symbol` | `lexical = 1.8`, `semantic = 0.6` | Per-class scaling of the lexical and semantic weights for symbol-shaped queries (a single identifier token) |
| `class_multipliers.path` | `lexical = 1.3`, `semantic = 0.8` | Same scaling for path-shaped queries (containing `/` or `\`) |
| `class_multipliers.signature` | `lexical = 1.4`, `semantic = 0.6` | Same scaling for signature-shaped queries (containing `(`, `->`, or `::`) |

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

Query classification (TASK-095): every pipelined query is classified by
shape — first-match signature (contains `(`, `->`, or `::`), then path
(contains `/` or `\`), then symbol (a single `[A-Za-z0-9_]+` token),
else conceptual — and the class scales ONLY the lexical and semantic
weights by the multipliers above, so structural signals are
class-independent. The conceptual class is the neutral 1.0/1.0 baseline
and has NO multiplier entry: a `[rank.class_multipliers.conceptual]`
table is a hard configuration error (its neutrality cannot be configured
away). The detected (or pinned) class is recorded on every pipeline row
as `query_class` and printed as one `query-class: <class>` stderr line
under `--why`, so a misclassification is diagnosable; the class affects
the blend only, never which signals run. `wonk search --query-class
<symbol|path|signature|conceptual>` (and the MCP `query_class`
parameter) pins the class explicitly, bypassing detection for that
invocation.

**`[duplicate]`**

| Key | Default | Description |
|-----|---------|-------------|
| `threshold` | `0.85` | Sketch-Jaccard level strictly above which two symbols are near-duplicates. Finite and in `(0, 1]`, else a hard configuration error naming the key |

Near-duplicate similarity (TASK-100) is lexical, not embedded: every
indexed symbol body is sketched at index time into a compact bottom-64
shingle signature (the `symbol_shingles` table), and query-time
similarity compares signature blobs only — no symbol body is ever read
to rank. The threshold feeds three consumers: the `novelty` demotion
pass (see `[rank] weights.novelty`), the `near_duplicates` pairs
recorded best-effort after each ranked search, and `wonk duplicates`
reporting (which also takes a one-off `--threshold` override).

**`[feedback]`**

| Key | Default | Description |
|-----|---------|-------------|
| `enabled` | `false` | Kill switch for usage-feedback capture. `false` records no slates and accepts no feedback; `true` also opts ranked search into the signal pipeline — the same implication as `--why` — because a legacy-path slate carries no signal contributions to learn from |
| `slate_retention` | `64` | Most recent slates kept per repo; older slates LRU-pruned in the same transaction as each insert. Must be at least `1`: `0` is a hard configuration error naming the key |
| `author_features` | `true` | Whether author-derived features (`last_touched_by`, `primary`) are recorded per slate member. `false` never builds the `author` group — no author data is written at all — and leaves every other recorded feature untouched |
| `learn_step` | `0.02` | Learning rate: the per-event fraction of each feature's contrastive advantage applied to its weight. Must be finite and `> 0`. Tuned via the deterministic sweep in `bench/feedback-learning-tuning.md` (OQ-019) |
| `learn_max_deviation` | `0.5` | Maximum deviation a learned weight may take from its default: multiplicative `[d·(1−dev), d·(1+dev)]` for signal names, `±dev` around zero for descriptive keys. Must be in `(0, 1]` — beyond `1.0` a signal weight could flip sign. The bound re-clamps stored values at load time, so tightening it immediately re-bounds what was learned (AR-043) |
| `learn_half_life_days` | `30` | Age in days over which an unrefreshed learned weight halves its distance from the default (PRD-FB-REQ-011). Must be `>= 1` |
| `learn_min_observations` | `10` | Observations a (feature, scope) row needs before it influences ranking (PRD-FB-REQ-025). Must be `>= 1` |
| `learn_min_sessions` | `3` | Distinct sessions a row needs before it influences ranking — one session repeating feedback never steers ranking (AR-044). Must be `>= 1` |

Usage-feedback capture (TASK-101): when enabled, every ranked search
persists its slate — the full ranked result list, each entry carrying a
content-anchored identity and the per-signal contributions `--why`
renders — and `wonk feedback` (or the `wonk_feedback` MCP tool) reports
which results were useful against that slate by token. Everything stays
in the per-repo index DB; there is no telemetry path. The section layers
per field like every other (per-repo over global over default), and
`slate_retention = 0` fails configuration load outright: no slate could
survive for the feedback call to reference. Default off — a
default-config search writes nothing and behaves byte-identically.

Result feature extraction (TASK-105, DR-043): each recorded slate
member also carries descriptive feature groups alongside its signal
contributions — hierarchical path features (one per ancestor directory,
plus path character, depth, and language), symbol attributes (kind,
scoping, name-match shape, bucketed body size), match shape (category,
term coverage, anchoring), graph position (bucketed fan-in/fan-out from
the persisted topology), modification history (bucketed recency and
churn, plus author facts), and, when a working-context hint was
supplied, context-relative features (same file, same directory, same
community, import-graph distance, co-change with the open file).
Continuous properties are recorded as fixed buckets, never raw values,
and per-slate categoricals beyond 32 distinct values collapse into a
shared `__overflow__` bucket. The caller supplies the hint as
`wonk search --context <PATH>` (CLI) or the optional `context_file`
argument to `wonk_search` (MCP); it feeds features only and never
affects ranking. Without a hint the context group is absent from the
recorded slate, not defaulted.

Contrastive weight learning (TASK-102, DR-042/DR-043): recording
feedback also learns. Each qualifying event — the useful result was not
already ranked first — moves every observable feature one bounded step
along its contrastive advantage (useful result minus the passed-over
alternatives), per query class and overall, with the weights stored in
the per-repo index and decaying back toward their defaults with age. A
row influences ranking only once it clears `learn_min_observations`
across `learn_min_sessions` distinct sessions and its decayed value
still differs from the default; below the gate the row is displayed
(`wonk feedback --weights`) but inert. A repository with no feedback, or
none past the gate, ranks exactly as with the feature disabled
(PRD-FB-REQ-020). Learning runs synchronously in the feedback call,
best-effort: a failure warns and leaves the events for the next call.
The rule and its constants are recorded in
`bench/feedback-learning-tuning.md`.

Author features exist behind their own switch because they deserve their
own decision (AR-046). The DR-039 distinction: DR-039 excludes
*assuming* authorship predicts relevance — baking an author-derived
ranking signal in without evidence. These features are the opposite
shape: explicit git-derived *evidence* recorded for learning, inert
until TASK-102's minimum-observation gate lets this repository's own
feedback produce a weight, inspectable with its supporting observation
count, and individually switchable. Nothing is assumed until the
feedback says so; a repository that does not want author data recorded
at all sets `author_features = false` and the `author` group is never
written.

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
