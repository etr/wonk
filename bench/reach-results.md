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
