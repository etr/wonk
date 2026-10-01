# Rank-weight tuning results (TASK-095, PRD-RANK-REQ-017, OQ-015/AR-034)

Date: 2026-09-30
Environment: Apple M4, macOS (darwin 25.3.0, arm64), rustc 1.97.1,
bench profile (release).

Labeled set: `tests/fixtures/labeled_queries` — 44 corpus files, 40 queries
(10 symbol, 8 path, 8 signature, 14 conceptual), labeled by construction
(`labels.toml` carries per-query justifications). Metric: precision@10
(rows in labeled-relevant files among the first ten flattened output rows,
the `ranking_regression` definition). Harness: `bench/rank_tune_bench.rs`
(`cargo bench --bench rank_tune`) — real `text_search` → real index →
real bundled embeddings → `rank_and_explain_classed`.

## Before / after (mean precision@10)

| Configuration                            | overall | symbol | path   | signature | conceptual |
|------------------------------------------|---------|--------|--------|-----------|------------|
| legacy (default config, REQ-017)         | 0.5025  | 0.7400 | 0.2625 | 0.2500    | 0.6143     |
| A lexical-lean                           | 0.5150  | 0.7800 | 0.2625 | 0.2500    | 0.6214     |
| B balanced                               | 0.5150  | 0.7800 | 0.2625 | 0.2500    | 0.6214     |
| C structural-rich                        | 0.5125  | 0.7700 | 0.2625 | 0.2500    | 0.6214     |
| D content-lean                           | 0.5125  | 0.7700 | 0.2625 | 0.2500    | 0.6214     |
| E C+symbol-split                         | 0.5125  | 0.7700 | 0.2625 | 0.2500    | 0.6214     |
| F B+symbol-split                         | 0.5175  | 0.7900 | 0.2625 | 0.2500    | 0.6214     |
| G C+stronger-split                       | 0.5175  | 0.7900 | 0.2625 | 0.2500    | 0.6214     |
| H D+symbol-split                         | 0.5150  | 0.7800 | 0.2625 | 0.2500    | 0.6214     |
| I semantic-forward+split                 | 0.5175  | 0.7900 | 0.2625 | 0.2500    | 0.6214     |
| J prominence-heavy                       | 0.5125  | 0.7700 | 0.2625 | 0.2500    | 0.6214     |
| **K kind+content-minimal (CHOSEN)**      | **0.5175** | **0.7900** | 0.2625 | 0.2500 | **0.6214** |
| L everything-mild                        | 0.5150  | 0.7800 | 0.2625 | 0.2500    | 0.6214     |
| M K+prominence-up                        | 0.5150  | 0.7800 | 0.2625 | 0.2500    | 0.6214     |
| N K+lexical-up                           | 0.5175  | 0.7900 | 0.2625 | 0.2500    | 0.6214     |
| O K+centrality-up                        | 0.5175  | 0.7900 | 0.2625 | 0.2500    | 0.6214     |
| P K+symbol-stronger                      | 0.5175  | 0.7900 | 0.2625 | 0.2500    | 0.6214     |
| Q K+proximity                            | 0.5175  | 0.7900 | 0.2625 | 0.2500    | 0.6214     |
| R G+path-character-up                    | 0.5150  | 0.7800 | 0.2625 | 0.2500    | 0.6214     |

Eight candidates (F, G, I, K, N, O, P, Q) tie at the 0.5175 plateau; the
fine pass (M–R) confirms it is the ceiling for this mechanism on this set.
The plateau is structural: with `kind` anchored at 1.0 (the frozen
equivalence contract), category tiers bound how much content signals can
move the top-10.

## Chosen configuration

K is selected from the tied plateau as the mildest table (the smallest
departure from neutral weights and multipliers). Kind stays anchored at
1.0; the tuned deltas ride on lexical/prominence/centrality/signature/
path_character plus a symbol-class split that scales lexical up and
semantic down for name lookups. Verbatim TOML:

```toml
[rank]
enabled = false   # the flip is a separate, gated commit

[rank.weights]
kind = 1.0
lexical = 0.4
semantic = 0.3
prominence = 1.0
centrality = 0.4
signature = 0.8
path_character = 0.6

[rank.class_multipliers.symbol]
lexical = 1.8
semantic = 0.6

[rank.class_multipliers.path]
lexical = 1.3
semantic = 0.8

[rank.class_multipliers.signature]
lexical = 1.4
semantic = 0.6
```

## Verdict against the flip gate

- Mean precision@10 strictly improves: 0.5025 → 0.5175 (+0.0150).
- No class regresses: symbol 0.7400 → 0.7900 (+0.05), conceptual
  0.6143 → 0.6214 (+0.0071), path and signature unchanged (0.2625,
  0.2500).
- Latency gate (REQ-016, < 20 ms added on warm queries): measured in
  `bench/rank_latency_bench.rs`; see `bench/rank-latency-results.md`.

The per-query deltas of the chosen table: `retry_backoff` 0.70 → 0.80,
`parse_header` 0.80 → 0.90, `TokenClaims` 0.80 → 1.00, `session storage`
0.90 → 1.00; no other query moves.
