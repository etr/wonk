# Elision line-reduction measurement (TASK-091)

Recorded: 2026-09-30 · `cargo test per_command_line_reduction_figures -- --nocapture`
(debug profile)

## Machine

- Apple M4, 16 GB RAM, macOS 26.3.1

## Protocol

Body-heavy fixture in the TASK-090 generator shape: one `src/lib.rs` with 40
Rust functions, each a 17-line body (three plain lets, a `while` loop around
ten plain lets, two tail lines), committed as the initial state of a real git
repo and indexed. Each command runs twice through its production path —
once with `elide: None`, once with `elide: Some(Salience)` — differing in
that option alone. Lines are counted on the rendered payload: `show` counts
the source body lines actually returned per symbol; summary/context/review
count pretty-printed payload JSON lines. The test asserts what the table
records: elision never drops symbols, show's reduction is a majority, and
the other three payloads are byte-identical between the two runs.

## Per-command reduction (north-star metric, PRD-ELIDE-REQ-008)

| command | without --elide | with --elide salience | reduction |
|---|---|---|---|
| show (source body lines) | 680 | 280 | 58.8% |
| summary (pretty payload lines) | 344 | 344 | 0 (by construction) |
| context (pretty payload lines) | 21 | 21 | 0 (by construction) |
| review (pretty payload lines) | 12 | 12 | 0 (by construction) |

## Self-check

- show returns 40 symbols in both runs (elision never drops results).
- Salience keeps the skeleton: every `while` header and closing brace
  survives; each collapsed run reports its own count (partition invariant:
  stub counts + retained lines == original line count, asserted in
  `salience_gaps_report_their_own_counts`).
- summary/context/review outputs are byte-identical with and without the
  flag, asserted by the same test.

## Why summary/context/review are 0 by construction (honest reading of REQ-008)

Only `show`'s payload carries source bodies today: `summary` emits
signature + doc comment, `context` emits signature + refs + children,
`review` emits findings (message + SymbolRefs) — none read symbol bodies.
The flag is wired into all four commands uniformly (CLI, router, MCP), so
the engine applies at each command's source-emission point; for these three
that point does not exist yet, and the reduction is provably zero rather
than merely unmeasured. **Stale-spec note:** PRD §3.36's claim that
"context/review return complete implementations" does not match the
implemented behavior — they are signature-only as shipped. Recorded here
per DR-036 instead of inventing body-bearing payloads (out of scope for
TASK-091). If those commands gain body-bearing fields, they flow through
the same `elide_span_tree` seam and this table gains real rows.

## Elision latency (same host, debug build, concurrent CI load)

TASK-090's Bodies rebuild over an already-parsed tree (6000-line sources,
three languages): 1.4-3.5 ms. Salience adds the control-flow walk of every
body interior — the cost class of one additional full-tree cursor walk:
Rust 114 ms vs 103 ms baseline, TypeScript 111 ms vs 81 ms, Python 101 ms
vs 68 ms on a loaded host (release builds are ~20-30x faster; the
deterministic correctness asserts and the `matches!` kind lookup carry the
regression guard, not these absolute numbers). One parse per file at query
time, shared by every span from that file (PRD-ELIDE-REQ-010).
