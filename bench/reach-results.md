# Reach table measurement (TASK-080, OQ-012)

Recorded: 2026-09-28 · `cargo bench --bench reach` (release profile)

## Machine

- Apple M4, 16 GB RAM, macOS 26.3.1

## Repo shape

- 297 src files x 200-function chains + impl blocks (struct + 2 methods each),
  1 util file with a global hub, 1 mids file with 30 mid-tier hubs,
  3 test files x 200 test fns calling production code (excluded by the shared
  predicate).
- 61,219 symbols indexed. The global hub `util_trace` has ~5,941 production
  callers (fan-out cap territory); mid hubs ~10; leaves 0–1.

## Results

| Metric | Value |
| --- | --- |
| Build, reach disabled (structural only) | 1.74 s |
| Build, reach enabled (depth 3, cap 500) | 2.71 s (delta **0.97 s**) |
| Depth-3 blast, table path — p50 | 0.012 ms |
| Depth-3 blast, table path — p95 | 0.055 ms |
| Depth-3 blast, table path — **p100** | **0.227 ms** (acceptance: < 50 ms) |
| Depth-3 blast, live BFS — p50 | 0.018 ms |
| Depth-3 blast, live BFS — p95 | 0.264 ms |
| Depth-3 blast, live BFS — p100 | 53.353 ms (hub-dominated tail) |
| Table @ cap 500 | 178,744 rows, 1 truncated source, 4.22 MiB, 72.3 B/symbol |
| Table uncapped | 196,097 rows, 0 truncated sources, 4.62 MiB, 79.2 B/symbol |
| Uncapped rebuild (direct `build_reach`) | 451 ms |

Table size via SQLite `dbstat` over `reach` + `reach_truncated` + `reach_meta`.

## Reading (OQ-012)

- The table answers depth-3 blast in **0.23 ms worst-case** — roughly 200x
  faster than the BFS tail on the same samples, comfortably under the 50 ms
  acceptance bound. Median cases are near-parity because most symbols have
  tiny reach sets; the table's value concentrates exactly where BFS hurts
  (hubs).
- Precompute cost is **~16 ms per 1k symbols** (0.97 s over 61k) on the full
  build, i.e. ~56% overhead on the structural build for this repo shape.
- Storage is **~72 bytes per symbol** (2.9 reach rows per symbol) at the
  default cap; the cap saves ~9% of rows and bounds pathological fan-out
  (the 5.9k-caller hub records a 500-row prefix plus a truncation marker).
- Uncapped rebuild of the whole table takes ~0.45 s, so TASK-081's
  incremental recompute has a clear full-rebuild ceiling to beat.

## TASK-081: incremental repair on re-index (re-recorded 2026-09-29)

`cargo bench --bench reach`, same repo shape, 15 edit iterations per shape
through the daemon's real path (`reindex_file`). Four edit shapes:

- **leaf** — a 2-fn file nothing references (~2-source rebuild set);
- **chain-hub** — `src/mod_0.rs`, a 200-fn chain file (~205-source rebuild
  set);
- **mids** — `src/mids.rs`, the 30 mid-tier hubs plus the global hub
  (~31-source rebuild set, every source's traversal cap-hitting);
- **util** — `src/util.rs`, the global hub's own source row set (1-source
  rebuild set: the capped ~5.9k-caller recompute).

### Correction to the previous record

The previous section gated **pooled** p95 and recorded "both runs pass"
against PRD-DMN-REQ-009. That claim was percentile-dependent and
run-fragile: the requirement ("shall complete in less than 50ms") states
no percentile, the pooled p100 exceeded 50ms in every prior run
(50.95–65.5ms), and the worst per-shape p95 (mids, 48.3ms) left under 4%
headroom. The gate is now **per-shape p95 < 50ms with the percentile
stated explicitly**; p50/p99/p100 are reported ungated; there is no
pooled gate (pooling dilutes the worst shape). A work-budget guard (below)
backs the gate, so over-budget repairs degrade instead of running long.

### The work-budget guard

`finish_file_edit` refuses any repair whose rebuild set exceeds
`MAX_INCREMENTAL_REPAIR_SOURCES = 25` source names — before any writes —
and the existing pipeline degrade wiring marks the table stale in the same
transaction and commits anyway; queries then fall back to BFS
(PRD-REACH-REQ-007) until the next full rebuild. `begin_file_edit` takes a
fast path: when the pre-edit affected set alone exceeds the budget (the
rebuild set is a superset of it), the remaining captures are skipped so
the refusal itself stays cheap. That fast path is measured, not assumed:
the first guard implementation paid ~31ms of rebuild-set computation to
say no and pushed chain-hub's degraded reindex p95 to 58ms — over the
budget it was guarding — before the early-out brought it to 19.80ms on
that (fast) session and 28.85ms on the verification session below.

Threshold rationale, from the runs below:

- Repair cost runs ~1ms per rebuild source for cap-hitting traversals
  (~0.8–1.0ms on the fast session, ~2ms under load).
- Within-budget sampled shapes: leaf (~2 sources) and util (1 source) —
  reindex p95 1.70 / 11.86ms on the verification session (0.79 / 10.68 on
  the fast session); never near 50ms at p95 in any observed run.
- Over-budget sampled shapes: mids (~31 sources) and chain-hub (~205) —
  incremental reindex p95 measured 42–125ms and 63–82ms respectively
  across sessions on this same machine, i.e. inside the 50ms budget only
  on the fastest runs. A provisional budget of 50 kept mids incremental
  at p95 41.96ms on the fast session — passing by 16% — while the loaded
  session had measured the same shape at 125.26ms: a gate that keeps
  mids incremental passes only when the machine is fast, which is the
  fragility the review flagged. 25 puts mids on the degraded path, whose
  cost tracks parse+upsert and is load-insensitive.
- The bound sits above the largest rebuild set the correctness suites
  generate (19 sources, measured across the full unit test suite), so no
  suite edit degrades.

### Results (verification run, 2026-09-29; mid-speed session, uncapped rebuild 967ms)

| Shape | Guard | Effective reindex p50/p95 (ms) | p99/p100 (ms) | Reindex off (stale skip) p50/p95 (ms) |
| --- | --- | --- | --- | --- |
| leaf | within budget — incremental | 1.35 / **1.70** | 1.75 / 1.75 | 0.27 / 0.76 |
| chain-hub | TRIPPED — degraded | 22.02 / **28.85** | 45.91 / 45.91 | 11.47 / 16.58 |
| mids | TRIPPED — degraded | 8.08 / **14.53** | 19.14 / 19.14 | 1.82 / 7.13 |
| util | within budget — incremental | 9.11 / **11.86** | 44.44 / 44.44 | 0.18 / 1.75 |

All four per-shape p95 gates pass (gate: per-shape reindex p95 < 50ms,
PRD-DMN-REQ-009, percentile stated); the worst headroom is chain-hub at
28.85ms p95 against the 50ms budget (42%). Repair-only (paired on/off
delta) for the incremental shapes: leaf p50 1.02 / p95 1.31 ms, util p50
8.81 / p95 11.67 ms. "Degraded" rows measure the full trip cost per
iteration — rebuild-set computation up to the refusal, stale marker,
commit — with the table rebuilt between iterations outside the measured
window; in the daemon's steady state, later edits of the same file take
the off-path cost (begin/finish no-op while stale). The util p99/p100 tail
(44.4ms) is the single-source capped hub recompute — inside 50ms at p95
on every observed run (p95 10.7–30.5ms across sessions) but the shape to
watch, and the reason the gate is stated at p95 rather than p99.

### Prior runs retained (unfavorable numbers included)

Guarded fast session (2026-09-29; uncapped rebuild 450ms), same code:

| Shape | Guard | Effective reindex p50/p95 (ms) | p99/p100 (ms) | Reindex off p50/p95 (ms) |
| --- | --- | --- | --- | --- |
| leaf | within budget — incremental | 0.66 / 0.79 | 0.82 / 0.82 | 0.24 / 0.60 |
| chain-hub | TRIPPED — degraded | 12.62 / 19.80 | 23.11 / 23.11 | 11.00 / 15.38 |
| mids | TRIPPED — degraded | 7.07 / 8.33 | 11.80 / 11.80 | 1.83 / 6.81 |
| util | within budget — incremental | 8.38 / 10.68 | 42.46 / 42.46 | 0.18 / 1.71 |

Pre-guard incremental measurements on this machine, loaded session
(2026-09-29; uncapped rebuild 1.16s, ~1.2x slower than the verification
run above):

| Shape | Incremental reindex on p50/p95/p100 (ms) | Reindex off p50/p95 (ms) |
| --- | --- | --- |
| leaf | 1.75 / 2.08 / 2.20 | 1.00 / 1.88 |
| chain-hub | 66.81 / 82.03 / 83.17 | 32.53 / 42.70 |
| mids | 67.37 / 125.26 / 135.43 | 7.04 / 15.96 |
| util | 23.23 / 30.45 / 106.90 | 0.75 / 3.39 |

That session failed even the old pooled gate (pooled p95 106.81ms — the
gate the previous record reported as "both runs pass"). The TASK-081
executor's two faster sessions (retained from the previous record)
measured pooled p95 40.1–48.3ms with pooled p100 64.8–65.5ms and mids
reindex p95 up to 48.3ms: under the requirement's plain "< 50ms" reading
only if the percentile is p95 and the machine is fast.

### Reading (TASK-081)

- With the guard, every sampled shape's reindex p95 sits inside the 50ms
  budget on both guarded sessions (worst: chain-hub p95 28.85ms, 42%
  headroom, on the slower session): small rebuild sets repair
  incrementally (leaf, util); large ones degrade to parse+upsert plus a
  bounded refusal, and the table falls back to BFS until the next full
  rebuild — correct, never wrong data (PRD-REACH-REQ-007).
- The trade-off is explicit: after a guard trip, depth-3 blast queries
  run the live BFS (verification session: p50 0.033ms, p95 0.80ms,
  p99/p100 110.5/122.4ms hub-dominated) until something rebuilds the
  table. A repo whose typical edits touch more than ~25
  affected+predecessor sources will spend most of its time on BFS — the
  deliberate exchange of the 50ms re-index budget for the pre-TASK-081
  BFS query cost.
- Repair cost remains ~linear in rebuild-set size within the budget; the
  four sampled shapes are not a bound on per-source cost — the guard is.
- Lookups stayed correct through every measured iteration (asserted in
  the bench: fresh and `Some` after incremental repairs, stale and `None`
  after guard trips), and the guard's verdict was all-or-nothing per shape
  (asserted: 15/15 trips for chain-hub and mids, 0/15 for leaf and util —
  comment-only edits over an identical graph must not flip it); the off
  phase doubles as a live check of the REQ-007 skip path.

