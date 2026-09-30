# Review engine measurement (TASK-085)

Recorded: 2026-09-29 · `cargo bench --bench review` (bench profile, optimized)

## Machine

- Apple M4, 16 GB RAM, macOS 26.3.1

## Repo shape

Same synthetic shape as the reach bench, but with real git history: 297 src
files x 200-function chains, 1 util file with a global hub, 1 mids file with
30 mid-tier hubs, 3 test files calling chain heads — 60,031 symbols. The
initial commit is the base state the index reflects (built with reach
enabled), and a typical unstaged diff is layered on top: 8 files, 14 changed
symbols — one mid-chain removal with ~11 callers, one signature change with
a surviving caller, nine uncovered body edits, two test-covered body edits,
one addition with an empty radius.

## Self-check (asserted before any timing)

- 2 blocking findings (`breaking-change/removed-symbol-with-callers`,
  `breaking-change/signature-changed-with-callers`) + 11 warnings (nine
  uncovered body edits, the addition, and the signature-changed fn whose own
  depth-3 radius also contains no tests) -> verdict BLOCK.
- Reach-disabled sweeps assert findings identical to reach-enabled sweeps
  (the kill switch changes speed only).

## Typical-diff review latency (25 sweeps)

| configuration | p50 | p95 | p99 | p100 |
|---|---|---|---|---|
| reach enabled (gated) | 47.80 ms | 52.70 ms | 56.51 ms | 56.51 ms |
| reach disabled (ungated contrast) | 45.83 ms | 52.43 ms | 67.73 ms | 67.73 ms |

Gate: **p95 = 52.70 ms < 2 s** with reach enabled — ~38x headroom against
the PRD-REV acceptance criterion. Percentile stated explicitly (reach-bench
precedent: never gate an unstated percentile).

### Two-phase split (reach enabled)

| phase | p50 | p95 | p99 | p100 |
|---|---|---|---|---|
| change detection (`detect_changes_detail`, separate sweeps) | 42.46 ms | 45.95 ms | 48.77 ms | 48.77 ms |
| blast + rules (paired per-iteration deltas, full − detect) | 5.54 ms | 6.75 ms | 7.75 ms | 7.75 ms |

Change detection dominates: two `git diff` subprocesses plus tree-sitter
re-parse of the 8 touched ~200-symbol files. Blast + rules (up to two
`analyze_blast` calls per candidate symbol — table path for rule A context,
live BFS for the rule-B with-tests question) is ~13% of the run.

## Other timings (ungated)

- repo generation: 34.81 ms; index build (60k symbols, reach on): 2.23 s;
  diff application: 1.64 ms.
