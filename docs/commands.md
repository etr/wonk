# Command Reference

Full flag and example reference for every wonk command. For a quick overview, see the [command summary table](../README.md#commands) in the README.

## Global flags

These flags work with any command:

| Flag | Description |
|------|-------------|
| `--format <format>` | Output format: `grep` (default), `json`, or `toon` |
| `-q`, `--quiet` | Suppress hint messages on stderr |
| `--budget <N>` | Limit output to approximately N tokens (higher-ranked results preserved) |

## Search

### `wonk search <pattern>`

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
| `--why` | Explain each result's ranking: per-signal contributions and the final score. Implies smart ranked mode through the signal pipeline; conflicts with `--raw` and `--semantic`. The breakdown is printed to stderr (one `why:` line per result, so stdout stays pipe-clean) and embedded as a `why` object per row in `--format json`. Feedback state renders as its own contributions: `feedback` (learned descriptive weights) and `preference` (the session-gated per-result bonus, TASK-104), each `value*weight=weighted` |
| `--query-class <class>` | Pin the query class (`symbol`, `path`, `signature`, `conceptual`), bypassing detection for this invocation. Implies smart ranked mode; conflicts with `--raw` and `--semantic`. The class scales the lexical/semantic blend (see `[rank] class_multipliers`) |
| `--context <PATH>` | The file you are currently working in. Feeds the context-relative features of the recorded feedback slate (same file, same directory, same community, import distance, co-change) when `[feedback]` is enabled; never affects ranking. Absent by default — the context features are absent rather than defaulted |
| `--no-feedback` | Ignore learned feedback state — weights and per-result preferences — for this search: ranking reproduces the index alone exactly (TASK-103, PRD-FB-REQ-017). Slates still record — capture is not influence — so feedback can still be reported against the reproducible ranking. The `learned:` `--why` line does not print |
| `-- <paths>` | Restrict search to specific paths |

When `[feedback] enabled = true` (default off, see
[Usage feedback](#usage-feedback)), a smart-ranked search also appends one
trailing `slate: <token>` line after the results — the handle
`wonk feedback` takes; stdout keeps its grep shape, so the line is
machine-cuttable. With feedback disabled the output is unchanged.

### `wonk ask <query>`

Semantic search: find symbols related to a natural language query.
Uses the bundled offline embedding provider by default. Configure Ollama with
`nomic-embed-text` only when opting into the external quality tier.

```
wonk ask "error handling logic"
wonk ask --from src/api.rs "authentication"
wonk ask --to src/db.rs "query builder"
```

| Flag | Description |
|------|-------------|
| `--from <file>` | Restrict to symbols reachable from this file |
| `--to <file>` | Restrict to symbols that can reach this file |

Degraded modes:

- **Configured Ollama unreachable**: the query falls back to the bundled
  provider with a `warning:` on stderr instead of failing, so stopping
  Ollama mid-session only lowers result quality.
- **Stored vectors from a different provider**: the query is refused with a
  `vector space mismatch` error naming the exact re-embed command (for
  example `wonk update --force --provider bundled`). Wonk never silently
  searches — or rewrites — the wrong vector space, and the fallback above
  never applies across a mismatch.

## Symbol lookup

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

### `wonk show <name>`

Show the full source body of a symbol. For container types (class, struct,
enum, trait, interface), use `--shallow` to get the container signature plus
child signatures without bodies.

```
wonk show "processPayment"
wonk show --file src/billing.ts "processPayment"
wonk show --kind function "handle"
wonk show --shallow "MyClass"
wonk show "handle" --elide
wonk show "handle" --elide=bodies
```

| Flag | Description |
|------|-------------|
| `--file <path>` | Restrict results to a specific file |
| `--kind <kind>` | Filter by symbol kind (e.g. `function`, `class`) |
| `--exact` | Require exact match on symbol name |
| `--shallow` | Show container signature + child signatures without bodies |
| `--elide [bodies\|salience]` | Elide function bodies from source output (default off; bare = `salience`) |

#### Elision and shallow mode

`--elide` collapses function bodies in extracted source: `bodies` replaces
each body with a one-line stub reporting how many lines it replaced, and
`salience` (the default when the flag is given bare) additionally keeps the
control-flow skeleton — conditionals, loops, match arms — verbatim, with the
collapsed runs in between still reporting their counts. Retained lines are
never rewritten; output is unchanged when the flag is absent. The flag also
exists on `summary`, `context`, and `review` for uniformity: those payloads
are signature-only today, so it is provably inert there (the recorded
reduction figures live in `bench/elision-results.md`).

`--elide` and `--shallow` never compound (PRD-ELIDE-REQ-009, DR-017). For
container symbols `--shallow` wins and the rendering is the shallow one —
signature plus child signatures from the index; elision is skipped entirely
because shallow never reads source files. For every other symbol `--elide`
applies to the extracted span. That is the single documented rendering for
the combination.

## Code structure

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

### `wonk summary <path>`

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

## Call graph

Wonk tracks caller/callee relationships by analyzing which symbols appear
within other symbols' bodies. This call graph powers the `callers`, `callees`,
`callpath`, `flows`, `blast`, `changes`, and `context` commands.

### `wonk callers <name>`

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

### `wonk callees <name>`

Find all callees of a symbol (symbols referenced within its body).

```
wonk callees "main"
wonk callees --depth 2 "main"
```

| Flag | Description |
|------|-------------|
| `--depth <N>` | Transitive expansion depth (default: 1 = direct callees only, max: 10) |
| `--min-confidence <F>` | Minimum edge confidence threshold (0.0-1.0) |

### `wonk callpath <from> <to>`

Find the shortest call chain between two symbols via BFS traversal.

```
wonk callpath "main" "dispatch"
wonk callpath --min-confidence 0.7 "handleRequest" "writeDB"
```

| Flag | Description |
|------|-------------|
| `--min-confidence <F>` | Minimum edge confidence threshold (0.0-1.0) |

## Program analysis

### `wonk flows [entry]`

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

### `wonk blast <symbol>`

Analyze the blast radius of a symbol change. Shows all affected symbols grouped
by severity tier (WILL BREAK, LIKELY AFFECTED, MAY NEED TESTING) with a risk
level assessment. Integrates inheritance edges (extends/implements).

When the target symbol owns a provider contract (a route it registers, a
queue it subscribes, a proto method it implements) that consumers in other
indexed repos of the same `[contracts] workspace` call, a fourth tier —
CROSS-REPO IMPACT — is appended below every depth tier. Each entry folds the
consuming repo into the file field as `<repo>:<path>` (e.g.
`web-client:src/client.js`), so existing output shapes stay unchanged.
Resolution reads the central registry (`~/.wonk/repos`) at query time;
registry problems degrade to a hint and never affect the depth tiers.

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

### `wonk changes`

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

### `wonk context <name>`

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

## Change impact

### `wonk impact <file>`

Analyze symbol changes and find semantically impacted downstream code.

```
wonk impact src/lib.rs
wonk impact --since HEAD~5
```

| Flag | Description |
|------|-------------|
| `--since <commit>` | Analyze all files changed since this commit |

## Review

### `wonk review`

Review a diff scope: line-anchored findings for the changed symbols plus one
mechanically derived verdict — `BLOCK` (any blocking finding), `REVIEW` (any
warning), or `APPROVE` (clean). The verdict is data: the exit code stays 0
either way, so pipelines decide for themselves what to do with a `REVIEW`.

Three rule families run, each independently switchable via `[review]` in
[configuration](configuration.md):

| Family | Kind | Severity | Fires when |
|-------|------|----------|------------|
| A — breaking change | `breaking-change` | blocking | A removed or signature-changed symbol still has indexed callers |
| B — coverage gap | `coverage-gap` | warning | An added/modified non-test symbol has no test file in its blast radius |
| C — cross-repo impact | `cross-repo` | warning | An added/modified/removed non-test symbol provides (when removed: provided) a contract consumed by another indexed repo |

```
wonk review                       # unstaged working-tree diff
wonk review --scope staged        # staged edits
wonk review --since main          # everything since a ref (compare sugar)
wonk review --scope compare --base main
```

| Flag | Description |
|------|-------------|
| `--scope <scope>` | `unstaged` (default), `staged`, `all`, or `compare` |
| `--base <ref>` | Base git ref (required when `--scope=compare`) |
| `--since <ref>` | Sugar for `--scope=compare --base=<ref>` |

Grep output — one line per finding plus a verdict line:

```
src/lib.rs:1 [BLOCKING] breaking-change: removed function `used` still has 1 indexed caller(s): caller
src/routes.js:2 [WARNING] cross-repo: function `registerUserRoutes` changed but provides contract(s) http::GET::/v1/users consumed by 1 other repo(s): own-api
verdict: REVIEW
```

Unanchored findings (no honest line could be determined) print `[unanchored]`
in place of `file:line` and are still emitted.

`--format json` is NDJSON: one independently-parseable line per finding plus
exactly one final verdict line — a clean diff still emits the verdict line.
Discriminate lines by key presence: the verdict line is the only one carrying
`verdict`.

```
{"file":"src/lib.rs","line":1,"anchor_method":"old-side-line","severity":"blocking","kind":"breaking-change","rule":"breaking-change/removed-symbol-with-callers","message":"...","identity":"...","related":[...]}
{"scope":"unstaged","verdict":"BLOCK","finding_count":1}
```

`--format toon` renders the whole result as one structured document.

**Boundaries (DR-035).** Findings are emitted only — nothing is posted to a
forge (no PR comments, no statuses) and nothing is auto-fixed. Review tells
you what the diff does; acting on it stays with you.

**Index currency caveat.** The index must reflect the base state of the diff.
Review therefore never auto-initializes an index: re-indexing the current
tree mid-diff would empty the diff and fake an `APPROVE`. Run `wonk init`
before starting your edits; if the index is missing, review fails loudly
instead of silently approving.

**Cross-repo prerequisites.** Rule C resolves against same-workspace repos in
the central registry (`~/.wonk/repos`), exactly like `wonk contracts --links`:
both repos must be indexed, and both must declare a `[contracts] workspace`
that intersects. The workspace is resolved once per run; if resolution fails
(review runs fail-soft), rules A/B still report and a single warning explains
what was skipped.

## Service contracts

### `wonk contracts`

List the service contracts indexed for this repository — HTTP routes,
environment variables, queues, gRPC methods, and the code that consumes
them — with workspace-aware status on consumer rows. By default every row
prints in the grep-compatible shape; unmatched consumers additionally carry
a `status=` token: `orphan` when this repo declares a workspace but nothing
in it serves the call, `unscoped` when no workspace is declared at all (a
configuration gap, never a defect). Providers and matched consumers carry
no token.

```
wonk contracts
wonk contracts --kind http --role provider
wonk contracts --orphans
wonk contracts --links
wonk contracts --unused-providers
```

Sample rows:

```
src/routes.js:2:http::GET::/v1/users role=provider confidence=1.0
src/client.js:2:http::GET::/v1/orders role=consumer confidence=1.0 status=orphan
```

| Flag | Description |
|------|-------------|
| `--kind <kind>` | Filter by contract kind: `http`, `env`, `queue`, `websocket`, `job`, `grpc`, `graphql`, `openapi` |
| `--role <role>` | Filter by role: `provider` or `consumer` |
| `--orphans` | Only list consumers with no provider within this repo's workspace (replaces the default row list) |
| `--links` | List resolved cross-repo provider<->consumer pairs annotated with both repo names (takes precedence over `--orphans`/`--unused-providers`) |
| `--unused-providers` | List providers with no consumer in this repo's workspace (replaces the default row list; off by default to avoid public-API noise) |

`--links` rows render both endpoints with their owning repo:

```
users-svc:src/app.js:2:http::GET::/v1/users role=provider <-> own-api:src/client.js:2 role=consumer basis=exact
```

Cross-repo rows resolve live at query time (nothing is persisted) against
same-workspace repos in the central registry (`~/.wonk/repos`): a sibling
repo participates only when the `[contracts] workspace` declared in its
`.wonk/config.toml` intersects this repo's. See
[configuration](configuration.md) for the `[contracts]` section.

Every run also prints workspace context hints on stderr (suppressed by the
global `--quiet` flag): the effective workspace set with co-members, the
exact config line to add when the workspace is undeclared —

```
workspace: my-api (undeclared — add 'workspace = "my-api"' under [contracts] in .wonk/config.toml to link sibling repos)
```

— and a `run wonk update` nudge when the workspace stored in the index no
longer matches the declared set.

## Near-duplicate detection

### `wonk duplicates`

Report groups of near-duplicate symbols — copy-pasted code whose lexical
bodies are almost identical (TASK-100). Similarity is computed from
compact shingle signatures stored at index time; this command sweeps the
whole index through hash buckets (never all-pairs) and prints each
connected group above the threshold:

```
wonk duplicates
wonk duplicates --threshold 0.9
```

Sample output:

```
dup-group 1 size=5 mean-sim=1.00
  src/handlers/a.rs:1 function handle_user_created
  src/handlers/b.rs:1 function handle_user_created
```

| Flag | Description |
|------|-------------|
| `--threshold <f32>` | Similarity override in `(0, 1]`; default from `[duplicate] threshold` (0.85) |

Groups are ordered by size (largest first), members by `(file, line)`;
singletons never appear. Mass-generated boilerplate can produce oversized
buckets — those compare only their first 1024 members and the run prints
a stderr note that more duplicates may exist. Pairs the sweep finds are
recorded into the index (`near_duplicates`), alongside the pairs each
ranked search with a `novelty` weight records best-effort. Text output
only — the grep-shaped lines are machine-cuttable; JSON is a follow-up.

## Usage feedback

### `wonk feedback`

Report which search results were useful, against the slate that search
persisted (TASK-101). A smart-ranked search with `[feedback] enabled =
true` records its full result list — every result's content-anchored
identity and per-signal contributions — in the per-repo index and echoes
a `slate:` token (see [Search](#search)); this command marks the useful
entries so future ranking work can learn from them. Nothing leaves the
repo's index DB — there is no telemetry path.

```
wonk feedback --slate 3f9c2a1b8d4e7f60 --useful 1 --session fix-auth
wonk feedback --slate 3f9c2a1b8d4e7f60 --useful 1,3 --session fix-auth
wonk feedback --slate 3f9c2a1b8d4e7f60 --useful 2 --useful <identity> --session fix-auth
```

Sample output:

```
recorded 2 event(s) against slate 3f9c2a1b8d4e7f60 (query "handleRequest", class symbol)
rank 1  src/handlers/a.rs:10  handle_user_created  [useful]
rank 2  src/api.rs:42  handle_request  [useful]
```

| Flag | Description |
|------|-------------|
| `--slate <token>` | Slate token from the search output (`slate:` line or JSON field) |
| `--useful <ranks-or-identities>` | Results that were useful: 1-based ranks or 64-hex identities; repeatable and comma-separated |
| `--session <id>` | Stable id for your current session/conversation (distinct sessions are counted separately) |
| `--weights` | List the learned weights instead of recording: every `learned_weights` row — gated and inert — with its default, observation count, and session count. Mutually exclusive with the three recording flags |
| `--list` | List the recorded feedback events instead of recording: one line per event (`#id rank N file:line symbol session S class C`, with `[retired]` when the identity no longer resolves). `--format json` emits the event summaries as objects |
| `--export` | Dump the complete event store — features payloads included — as one JSON array to stdout (`wonk feedback --export > events.json` to save). Round-trips the store verbatim |
| `--clear-events` | Wipe EVERY recorded feedback event. Learned state — weights and per-result preferences — is untouched: clearing history and resetting weights are independent operations (PRD-FB-REQ-013) |
| `--clear-result <IDENTITY>` | Wipe one result's recorded events by its 64-hex identity. Learned state — weights, and that result's preference — is untouched (the per-result kill is `--reset-weights`) |
| `--reset-weights` | Reset ALL learned weights to their configured defaults (every scope) and clear every per-result preference — both are learned state and reset together (TASK-104). The event history is untouched: recorded events stay processed and never silently re-teach the wiped state |
| `--reset-weight <FEATURE>` | Reset ONE feature's learned weights to defaults (e.g. `path_character`), all of its scopes; sibling features and the event history stand |

All three recording flags are required together (or none, with one of
the management flags above), and `--useful` must name at least one
result; a result named twice (by rank and by identity) records once.
The management flags are mutually exclusive with each other and with
recording, and they work regardless of `[feedback] enabled` —
inspecting and wiping leftover state after opting out is exactly when
they matter. Only recording requires the feature on.

Each recorded event also drives contrastive weight learning
(TASK-102): features that scored the useful result above the passed-over
alternatives gain weight, bounded by
`[feedback] learn_max_deviation` and decaying toward the defaults with
age. Inspect what the repository has learned with `wonk feedback
--weights`:

```
$ wonk feedback --weights
kind [overall] 1.000 (default 1.000) 24 obs, 24 sessions
path_character [overall] 0.150 (default 0.100) 40 obs, 40 sessions
path:tests [overall] -0.400 (default 0.000) 40 obs, 40 sessions
lexical [overall] 0.340 (default 0.400) 12 obs, 12 sessions [below gate]
```

`[overall]` is the all-queries scope; `[symbol]`/`[path]`/`[signature]`
rows carry the per-query-class adjustments. The effective value is
decayed to now, so an untouched row drifts back toward its default
visibly. `[below gate]` marks rows that have not cleared
`learn_min_observations` across `learn_min_sessions` distinct sessions —
legible evidence, no ranking influence. `--format json` emits the rows
as objects (`feature`, `query_class`, `effective`, `default`,
`observations`, `sessions`, `gated`). Under `--why`, a live search also
prints a `learned:` line to stderr naming the gated weights in effect
for that query's class.

The same events also grow session-gated per-result preferences
(TASK-104, PRD-FB-REQ-016): a result confirmed useful across
`prefer_min_sessions` distinct sessions (default 3) gains an additive
preference shown as its own `preference` contribution in `--why` —
distinct from the `feedback` row the learned weights produce — capped at
half that channel's swing, decaying on the same half-life, and never
counting rank-1 picks or same-session repeats. A materially changed
result yields a new identity, so its stored preference stops applying
without any write.
Recording requires `[feedback] enabled = true` in `.wonk/config.toml`
(otherwise the command fails with `feedback capture is disabled; set
[feedback] enabled = true in .wonk/config.toml`) and an existing index
(`wonk init`). A token past its `[feedback] slate_retention` window
fails with `slate not found (expired or pruned); re-run the search and
report against the new slate`; an unknown rank or identity fails with a
`not in the slate` error. Arguments are validated before the first
write, so a failed call records nothing. Identities anchor on the owning
symbol's file, kind, name, and signature — they survive body-only edits
and re-indexing, and retire on renames or signature changes; a retired
entry is reported at record time as a `note: <identity> no longer
resolves in the index; the entry will not apply` line.
`--format json` prints the summary as an object (recorded count, query,
class, and per-event identity/rank/file/line/symbol/liveness).

## Semantic

### `wonk cluster <path>`

Cluster symbols by semantic similarity within a directory.
Uses K-Means with automatic K selection via silhouette scoring.

```
wonk cluster src/
wonk cluster --top 3 src/components/
```

| Flag | Description |
|------|-------------|
| `--top <N>` | Representative symbols per cluster (default: 5) |

## Index management

### `wonk init`

Manually initialize indexing for the current repository. This is optional --
any query command automatically builds the index on first use.

```
wonk init
wonk init --local
wonk init --provider ollama
```

| Flag | Description |
|------|-------------|
| `--local` | Use a project-specific index instead of the shared index |
| `--provider <bundled|ollama>` | Override the configured embedding provider for this build |

### `wonk update`

Re-index the current repository.

```
wonk update
wonk update --force --provider ollama
```

| Flag | Description |
|------|-------------|
| `--force` | Force a full structural and embedding rebuild |
| `--skip-embed` | Update only the structural index |
| `--provider <bundled|ollama>` | Override the configured embedding provider for this build |

### `wonk status`

Show indexing status for the current repository, including the active
embedding provider, the stored vector space, and workspace membership.

```
wonk status
```

Sample output:

```
Index: 42 files, 300 symbols, 1200 references
Workspaces: payments (co-members: users-svc, web-client)
Embeddings: 280 embeddings (3 stale)
Provider: bundled
Stored vectors: bundled, 256-dim
```

The `Workspaces:` line reports the effective workspace set — the
`[contracts] workspace` ids declared in repo-local `.wonk/config.toml`, or
the repository's own name when nothing is declared (shown as
`(undeclared)`). Sibling indexed repos sharing a workspace list as
co-members; a repo with a mistyped workspace shows as a singleton.

When Ollama is configured (or ollama vectors are stored), an `Ollama:` line
reports reachability and, when unreachable with Ollama configured, notes that
semantic queries fall back to the bundled provider. `Stored vectors: none`
means the index has no embeddings yet.

A `Feedback:` line summarizes the usage-feedback loop (TASK-103/104):
`Feedback: enabled, 40 events, 40 sessions, weight deviation 0.050, 3
result preferences` — event count, distinct sessions among them, the
current weight deviation (the largest `|effective − default|` over
gated learned rows, bounded by `[feedback] learn_max_deviation`), and
the stored per-result preference count. With the feature
off the line reads `Feedback: disabled`, still showing the counts when
leftover state exists — turning the feature off hides influence, not
history.

With `--format json` (or the MCP `wonk_status` tool) the same data is
serialized, including `active_provider`, `stored_vector_provider`,
`stored_vector_dim`, `ollama_reachable` (`null` when Ollama was not
probed), and the workspace fields: `workspaces` (effective set),
`workspace_declared` (whether `[contracts] workspace` is set in repo-local
config), and `workspace_comembers` (names of other indexed repos sharing a
workspace), plus the feedback state under `feedback` (`enabled`, `events`,
`sessions`, `deviation`).

### `wonk repos <list|clean>`

Manage tracked repositories.

```
wonk repos list
wonk repos clean    # Remove stale repositories from the index
```

## Daemon

### `wonk daemon <start|stop|status|list>`

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

## Integration

### `wonk mcp serve`

Start an MCP (Model Context Protocol) server over stdio. This lets AI coding
assistants like Claude Code use wonk as a tool provider.

```
wonk mcp serve
```

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

Smart mode ranks through the signal pipeline by default: each result's
score is a weighted sum of per-signal contributions (kind tier, BM25
lexical, embedding semantic, caller centrality, name prominence, path
character, term proximity, signature match). The weights and the
per-query-class scaling of the lexical/semantic blend are configurable
via `[rank]` (see `docs/configuration.md`); the default table is tuned
against a labeled query set. Every pipelined query is classified by
shape — signature, path, symbol, or conceptual — and the class is
recorded as a `query_class` field on every JSON row so a
misclassification is diagnosable. `--query-class <class>` pins the class
explicitly. Setting `[rank] enabled = false` restores the pre-pipeline
tier ordering byte-for-byte.

With `[feedback] enabled = true` (default off, see
[Usage feedback](#usage-feedback)), a smart-ranked search additionally
stamps every JSON row with `slate` (the persisted slate's token) and
`identity` (the row's content-anchored identity) so an agent can report
useful results by identity. Both fields are absent when feedback is
disabled, so disabled JSON output is unchanged.

## Semantic search

Wonk supports embedding-based semantic search with a bundled in-process
provider by default. You can opt into [Ollama](https://ollama.ai/) with the
`nomic-embed-text` model for a higher-quality tier. Both let you search by
meaning rather than exact text patterns.

- **Default setup**: No external embedding service is required
- **Ollama tier**: Set `[embedding] provider = "ollama"` and pull
  `nomic-embed-text` (`ollama pull nomic-embed-text`)
- **Embedding build**: Embeddings are built on first semantic query or explicitly via `wonk init`
- **Freshness**: The background daemon keeps embeddings up to date as files change
- **Vector-space safety**: Provider and dimension are stored with every vector;
  a mismatch stops the query and prints the exact `wonk update --force
  --provider ...` command needed to rebuild
- **Unreachable-provider fallback**: A configured Ollama that is down at query
  time degrades to the bundled provider with a stderr warning instead of
  failing the query. The fallback only applies when the stored vectors are
  compatible (bundled or none); if any foreign-space vectors are stored, the
  query blocks with the re-embed command above — so switching providers is
  always an explicit `wonk update --force --provider <bundled|ollama>`
- **Dependency scoping**: Use `--from <file>` and `--to <file>` to restrict
  semantic results to symbols reachable from or leading to a specific file,
  using the indexed dependency graph
- **Hybrid fusion**: `wonk search --semantic` blends structural and semantic
  result lists using Reciprocal Rank Fusion (RRF). The fusion constant K
  is configurable via `[search] rrf_k` (default: 60.0); higher values produce
  more even blending. The lexical input to fusion is BM25-ranked via index
  term statistics (`[search] bm25_k1` / `bm25_b`, defaults 1.2 / 0.75);
  indexes built before V5 lack those statistics, so the list fuses in the
  previous (match-presence) order with a hint to run `wonk init`

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
