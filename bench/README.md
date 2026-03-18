# Token Savings Benchmark

Compares `wonk search` against raw `rg` (ripgrep) across 5 real open-source
codebases. Both tools run from the repo root, producing `file:line:content`
output so byte/token comparisons are apples-to-apples.

## Prerequisites

- `wonk` on PATH (or set `WONK=/path/to/wonk`)
- `rg` (ripgrep) on PATH (or set `RG=/path/to/rg`)
- `git` on PATH
- ~2 GB disk for cloned repos

## Usage

```bash
# From the repo root:
cargo build --release
cd bench
./token_bench.sh
```

The script will:

1. Shallow-clone 5 repos to `bench/repos/` (skips if already present)
2. Run `wonk init` on each repo
3. Run 5 queries per repo through both `rg` and `wonk search`
4. Run a budget sweep (500/1000/2000/4000 tokens) for each query
5. Print markdown tables to stdout and save to `bench/results/report.md`

## What it measures

| Metric | Description |
|--------|-------------|
| Token count | `ceil(bytes / 4)` — same heuristic wonk uses internally |
| Line count | Number of result lines from each tool |
| Reduction % | `(rg_tokens - wonk_tokens) / rg_tokens * 100` |
| Defs found | Whether wonk's `-- definitions --` section appeared |
| Dedup count | Number of `(+N other locations)` annotations |
| Budget fit | What % of raw rg output fits in a given token budget |

## Repos and queries

| Repo | Language | Queries |
|------|----------|---------|
| BurntSushi/ripgrep | Rust | search, match, regex, parse, printer |
| tokio-rs/tokio | Rust | spawn, runtime, task, poll, waker |
| pallets/flask | Python | route, request, response, app, Blueprint |
| django/django | Python | Model, QuerySet, view, middleware, Field |
| expressjs/express | JavaScript | Router, middleware, request, response, next |

## Runtime

Expect ~5 minutes on first run (dominated by cloning django at ~250K LOC).
Subsequent runs skip cloning and take ~2 minutes.

## Output

Results are written to `bench/results/report.md` (gitignored) and also
printed to stdout.
