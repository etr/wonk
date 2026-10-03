# Review engine measurement (TASK-085)

Recorded: 2026-10-02 · `cargo bench --bench review` (bench profile, optimized)

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
| reach enabled (gated) | 121.41 ms | 126.66 ms | 131.25 ms | 131.25 ms |
| reach disabled (ungated contrast) | 122.09 ms | 125.88 ms | 128.90 ms | 128.90 ms |

Gate: **p95 = 126.66 ms < 2 s** with reach enabled — 15.8x headroom against
the PRD-REV acceptance criterion. Percentile stated explicitly (reach-bench
precedent: never gate an unstated percentile).

### Two-phase split (reach enabled)

| phase | p50 | p95 | p99 | p100 |
|---|---|---|---|---|
| change detection (`detect_changes_detail`, separate sweeps) | 121.00 ms | 123.56 ms | 126.18 ms | 126.18 ms |
| blast + rules (paired per-iteration deltas, full − detect) | 0.47 ms | 7.54 ms | 8.78 ms | 8.78 ms |

Change detection loads the selected old Git blob and selected new endpoint,
parses both snapshots at their own line coordinates, and reuses that metadata
in review. The full-review and detection calls are timed back to back on the
same iteration. Percentiles sort copies, preserving the raw pairing in
[`review-samples.json`](review-samples.json). The paired delta is a difference
between two timed operations, rather than a separately instrumented phase:
five of 25 deltas are negative (minimum -4.60ms) from scheduling/cache noise.
Negative samples are retained, and no isolated blast/rules percentage is inferred.

Conditions: Apple M4, 16 GiB RAM, macOS 26.3.1 arm64, Rust 1.97.1; optimized
bench profile. One untimed full-review correctness pass precedes 25 paired
full/detection samples, then 25 reach-disabled contrast samples. The source/Cargo
gate excludes other Rust workloads; no claim is made that all host processes
were idle. Fixture config uses defaults plus the explicit repository layer.

## Other timings (ungated)

- repo generation: 50.02 ms; index build (60k symbols, reach on): 3.58 s;
  diff application: 1.41 ms.
