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

## TASK-081: incremental repair on re-index (recorded 2026-09-29)

`cargo bench --bench reach`, same repo shape, 15 edit iterations per shape
through the daemon's real path (`reindex_file`). Four edit shapes:

- **leaf** — a 2-fn file nothing references (minimal affected set);
- **chain-hub** — `src/mod_0.rs`, a 200-fn chain whose edit's reverse
  lookup pulls in the `util_trace` hub recompute (~5.9k-caller query,
  capped at 500 rows);
- **mids** — `src/mids.rs`, the 30 mid-tier hub sources;
- **util** — `src/util.rs`, the global hub's own source row set.

**Repair-only** is the paired on/off delta: the same edit measured with the
table fresh (repair runs) and with the stale marker set (begin/finish skip
— the REQ-007 no-op path), isolating the repair's own cost from parse +
upsert.

Two runs on the same machine (Apple M4, macOS 26.3.1; this session ran
~1.2–1.5x slower than the TASK-080 recording above — uncapped rebuild
measured 540–697 ms vs the recorded 451 ms — so shape ratios, not absolute
times, are the comparison that matters):

| Shape | Reindex on p50/p95 (ms) | Reindex off p50/p95 (ms) | Repair-only p50/p95/p100 (ms) |
| --- | --- | --- | --- |
| leaf | 0.75–0.95 / 0.83–1.18 | 0.31–0.37 / 0.83–0.86 | 0.39–0.59 / 0.50–0.70 / ≤0.74 |
| chain-hub | 34.1–38.9 / 40.1–47.7 | 13.2–17.0 / 22.4–27.7 | 17.5–20.8 / 24.2–29.8 / ≤51.4 |
| mids | 26.9–34.2 / 35.4–48.3 | 2.2–2.8 / 7.9–10.5 | 24.6–31.5 / 33.2–45.8 / ≤55.0 |
| util | 10.2–13.1 / 14.8–16.1 | 0.20–0.27 / 1.95–2.38 | 10.0–12.7 / 14.6–15.8 / ≤62.9 |

**Pooled reindex with repair (the PRD-DMN-REQ-009 budget):**
p50 25.5–33.1 ms, **p95 40.1–48.3 ms** (< 50 ms gate, both runs), p100
64.8–65.5 ms. The gate in the bench is `ensure!(p95 < 50.0)`; both runs
pass.

### Reading (TASK-081)

- The **worst repair-only p95 is 33–46 ms (mids)** — the shape whose edit
  rebuilds 30 mid-tier sources and their ~35-target reach sets through the
  per-name SQL candidate lists. That is **within the 50 ms re-index
  budget** and roughly **10x faster than the 451 ms full-rebuild ceiling**
  (540–697 ms on today's slower session), so the incremental path is the
  right default for the daemon loop.
- Hub-shaped edits stay bounded by design: editing the chain-hub file
  re-runs the global hub's capped 500-row recompute (17–21 ms repair
  p50), and editing `util.rs` itself rewrites just the one hub source
  (10–13 ms repair p50) — the fan-out cap converts the pathological
  5.9k-caller case into a constant-bounded prefix.
- Leaf edits cost sub-millisecond repair; the bulk of any real re-index
  remains parse + upsert (13–17 ms of the chain-hub file's ~34–39 ms
  total).
- Lookups stayed `Some` and the table stayed fresh after every measured
  iteration (asserted in the bench); the stale-marked off phase doubles as
  a live check of the REQ-007 skip path.

