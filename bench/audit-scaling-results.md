# Audit remediation scaling measurements

Recorded 2026-10-02 on the repaired validation tree with
`cargo bench --bench audit_scaling` (optimized bench profile, `RUSTFLAGS="-D warnings"`).
All tables and raw samples below use this rerun; earlier measurements are superseded.
Apple M4, 16 GiB RAM, 10 logical CPUs, macOS 26.3.1 arm64; Rust 1.97.1.
Three untimed warmups and 25 timed samples per input; p95 is sorted sample 24 of 25.
The source/Cargo gate excluded competing Rust builds and tests. Other host processes
were not instrumented. Config was defaults plus the explicit fixture repo layer.
No models were downloaded. Raw timing/allocation samples and conditions are in
[`audit-scaling-samples.json`](audit-scaling-samples.json).

Allocation counts cover Rust System allocator requests, including reallocations.
Allocated bytes are cumulative requests during each operation, not resident or peak
memory; SQLite internal C allocations are excluded. Tables show median requests.

## F19: actual ranking context preparation

`rerank::prepare_context` with file churn, one candidate per indexed file.
Absolute paths use the MCP-shaped `/repo/<relative key>` form; relative paths are controls.
Each returned candidate is asserted to resolve to the expected churn score.

| Files / candidates | Path form | p50 ms | p95 ms | Rust allocations | Rust allocated bytes |
|---:|---|---:|---:|---:|---:|
| 2,000 | relative | 7.448 | 8.806 | 24,104 | 3,005,060 |
| 2,000 | absolute | 10.446 | 11.173 | 44,225 | 8,061,700 |
| 10,000 | relative | 36.141 | 38.831 | 120,216 | 14,377,800 |
| 10,000 | absolute | 52.033 | 57.962 | 220,365 | 38,768,520 |
| 20,000 | relative | 72.036 | 77.037 | 240,343 | 28,977,836 |
| 20,000 | absolute | 116.522 | 130.046 | 440,503 | 77,893,676 |

The actual preparation regression counts reverse-component and alias work:
64 absolute files previously used 20,480 operations and now meet the <=1,280
bound. The index is built once and paths/aliases are traversed once; the
20k absolute curve no longer exhibits the former candidate-by-corpus scan.

## F21: candidate-bounded BM25 transfers

Every corpus file contains the three common query terms `pub fn gamma`, but only
`src/file0.rs` is a candidate. Actual SQLite ROW tracing checks precisely three
transferred `(file, tf)` rows. DF is counted inside SQLite in one read snapshot.

| Corpus files | Candidates | Transferred postings | Posting data proxy B | p50 ms | p95 ms | Rust allocated B |
|---:|---:|---:|---:|---:|---:|---:|
| 2,000 | 1 | 3 | 60 | 0.235 | 0.257 | 1,672 |
| 10,000 | 1 | 3 | 60 | 0.943 | 1.192 | 1,672 |
| 20,000 | 1 | 3 | 60 | 2.026 | 2.558 | 1,672 |

The posting proxy is path bytes plus eight bytes for TF per transferred row;
it is not a process memory measurement. Gate: **warm p95 <10ms**, including
20,000 corpus files; all three scales passed. Global DF counting still performs
index work proportional to matching corpus rows, while Rust posting materialization
is candidate-bounded.

## F20: actual indexed relaxed RPC join

Qualified `users.v1.ServiceN::GetUser` providers pair with unqualified
`ServiceN::get_user` consumers; there are no exact-ID counterparts.

| Providers | Consumers | Exact relaxed matches | p50 ms | p95 ms | Rust allocations | Rust allocated B |
|---:|---:|---:|---:|---:|---:|---:|
| 1,000 | 1,000 | 1,000 | 1.180 | 1.521 | 19,051 | 1,387,062 |
| 2,000 | 2,000 | 2,000 | 2.467 | 2.917 | 38,056 | 2,785,030 |
| 4,000 | 4,000 | 4,000 | 4.894 | 5.522 | 76,061 | 5,580,942 |

The actual-code operation regression bounds indexed preparation and consumer
lookup by <=4(P+C), excluding output construction; reference golden fixtures
also preserve exact exclusions, wildcard/tie precedence and input ordering.

## F22: bounded normalization including distant closing delimiters

Repaired-tree measurements use the actual public normalizer. Unmatched shapes
have no closing partner. Distant-closer inputs are `fragment.repeat(N) + ">"`:
every segment-start angle sees the same final closer. They include empty, ASCII,
and UTF-8 names. Typed converter controls preserve the final name, including
converters with slashes before their final colon; slashes in the name are rejected.

| Shape | Repetitions | Input bytes | p50 ms | p95 ms | Rust allocated B | Accounted rewrite work |
|---|---:|---:|---:|---:|---:|---:|
| `unmatched {` | 16,000 | 16,003 | 0.118 | 0.120 | 112,022 | — |
| `unmatched ${` | 16,000 | 32,003 | 0.236 | 0.237 | 224,022 | — |
| `unmatched <` | 16,000 | 16,003 | 0.117 | 0.118 | 112,022 | — |
| `unmatched (.:` | 16,000 | 48,003 | 0.395 | 0.498 | 336,022 | — |
| `unmatched {` | 32,000 | 32,003 | 0.235 | 0.317 | 224,022 | — |
| `unmatched ${` | 32,000 | 64,003 | 0.456 | 0.458 | 448,022 | — |
| `unmatched <` | 32,000 | 32,003 | 0.220 | 0.332 | 224,022 | — |
| `unmatched (.:` | 32,000 | 96,003 | 0.793 | 0.916 | 672,022 | — |
| `unmatched {` | 64,000 | 64,003 | 0.445 | 0.534 | 448,022 | — |
| `unmatched ${` | 64,000 | 128,003 | 0.950 | 1.199 | 896,022 | — |
| `unmatched <` | 64,000 | 64,003 | 0.516 | 0.706 | 448,022 | — |
| `unmatched (.:` | 64,000 | 192,003 | 1.363 | 1.678 | 1,344,022 | — |
| `distant /<` | 16,000 | 32,001 | 0.309 | 0.313 | 224,008 | 95,999 |
| `distant /<a` | 16,000 | 48,001 | 0.479 | 0.589 | 432,109 | 127,998 |
| `distant /<é` | 16,000 | 64,001 | 0.628 | 0.802 | 576,105 | 143,999 |
| `distant /<` | 32,000 | 64,001 | 0.674 | 0.781 | 448,008 | 191,999 |
| `distant /<a` | 32,000 | 96,001 | 1.003 | 1.146 | 864,109 | 255,998 |
| `distant /<é` | 32,000 | 128,001 | 1.302 | 1.440 | 1,152,105 | 287,999 |
| `distant /<` | 64,000 | 128,001 | 1.393 | 1.705 | 896,008 | 383,999 |
| `distant /<a` | 64,000 | 192,001 | 1.964 | 2.168 | 1,728,109 | 511,998 |
| `distant /<é` | 64,000 | 256,001 | 2.674 | 3.084 | 2,304,105 | 575,999 |

The deterministic actual-code regression accounts for delimiter byte visits,
rewrite iterations, constant name validation, identifier scanning and successful
name copying. These are work units, not CPU instructions. Every variable-length
rewrite scan is counted; other public normalization stages are fixed linear
passes. The test bounds accounted work by <=12 times input bytes. Last-colon and
last-slash positions are collected during each monotonic delimiter pass, so
rejected overlapping names use constant-time validation. The measured distant
closer work doubles with input size. Source HTTP, WebSocket and OpenAPI extraction
controls exercise this same public normalizer. Timing is benchmark evidence,
never a correctness threshold in the ordinary regressions.

## F23: 11-repository workspace resolution

Eleven real indexed repositories, 40 contracts each: exactly 10 siblings, 40
cross-repo links and 20 linked own consumers are asserted on every sample.
Warm p50 **11.447ms**, p95 **13.269ms**. Gate:
**p95 <100ms** passed. Ordinary unit tests retain the deterministic identity/count
assertions and have no wall-clock correctness threshold.

## F09: delivered feedback storage and stamping

Actual MCP delivery selection uses a 4,000-token budget with feedback metadata
cost reserved. Only selected rows enter the stored slate and event feature payload;
every stamped output identity is checked against its corresponding member.

| Candidates | Delivered / stored | Member JSON B | Latest event features B | Corresponding stamp pairs | p50 ms | p95 ms |
|---:|---:|---:|---:|---:|---:|---:|
| 2,000 | 85 / 85 | 32,367 | 32,416 | 85 | 1.694 | 2.357 |
| 10,000 | 85 / 85 | 32,367 | 32,416 | 85 | 2.987 | 3.675 |
| 20,000 | 85 / 85 | 32,367 | 32,416 | 85 | 4.400 | 5.335 |

Selection still walks candidates in O(C); persistence and the corresponding
zip stamp operate on returned R. The delivered count and JSON byte sizes are
constant across these scales. This measures selection + store + stamp + one event,
not grep, embeddings, process startup or full end-to-end query latency.
