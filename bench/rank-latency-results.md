# Rank latency results (TASK-095, PRD-RANK-REQ-016)

Date: 2026-09-30
Environment: Apple M4, macOS (darwin 25.3.0, arm64), rustc 1.97.1,
bench profile (release).

Harness: `bench/rank_latency_bench.rs` (`cargo bench --bench rank_latency`).
The labeled corpus is indexed and embedded once with the bundled provider;
every query's candidate set is fetched once (warm cache, warm connection)
and given one untimed pass through both paths; then 60 timed iterations
measure the wall-clock of the classed pipeline (full tuned defaults —
lexical, semantic, prominence, centrality, signature, path_character all
active, including every context preparation) MINUS the legacy ordering,
summed over all 40 labeled queries per iteration.

## Measurement

- Mean ADDED latency, 40 queries per iteration: **8.309 ms**
  (~0.21 ms per query)
- p95: **11.131 ms**
- Run-to-run variance on this machine: repeated runs measured 10.451 ms /
  12.829 ms and 7.376 ms / 9.581 ms — comfortably inside the gate every
  time.
- Gate: mean < 20 ms per warm query — satisfied with an order of magnitude
  of headroom even on the aggregate sum.

The gate in REQ-016 is per warm query; the recorded mean is the sum over
the whole 40-query labeled set per iteration, a strictly harsher measure.

### Cold-start disclosure (review debt, 2026-10-02)

The REQ-017 default flip puts `semantic = 0.3` in the active default
weights, so a one-shot CLI `wonk search` on the Smart path decodes the
6.5MB bundled model artifact once per process (zstd decompress + unpack
+ `StaticModel::from_bytes`) before the first query. The warm-query
numbers above exclude it by construction (the bench warms untimed);
REQ-016 is scoped to warm queries, and the long-lived surfaces (daemon,
MCP server) amortize the decode across their lifetime.

Measured on this host (Apple Silicon, release binary, two-function
fixture, `/usr/bin/time -p`, 3 runs each): default-on smart search
70-80 ms wall vs 15-20 ms with `[rank] enabled = false` — a ~60 ms
cold-start delta dominated by the decode. On the TASK-095 session's
larger fixture corpora the recorded delta was ~290-515 ms (303-545 ms
mean vs 15-31 ms); absolute numbers scale with corpus and host.

Mitigation path if one-shot CLI cold start matters: persist the decoded
model as a memory-mappable artifact under `~/.wonk` (versioned by model
id) so subsequent processes mmap instead of decode. Not implemented:
the daemon and MCP server are the intended steady-state surfaces, and
`[rank] enabled = false` restores the fast legacy path for scripting.
