# Contract extraction build-cost results (TASK-082 + TASK-087 + TASK-088)

Synthetic repo: 12 languages x 25 files, ordinary symbols plus 2-5
framework idioms per file (HTTP/env from TASK-082; ~2 message-kind
idioms — queue/websocket/job — per file from TASK-087), plus a
TASK-088 document cohort: ~5k-line OpenAPI spec, proto IDL, GraphQL
SDL, and one large non-OpenAPI JSON (negative sniff case; stays
un-indexed).
`cargo bench --bench contracts`.

| metric | value |
|---|---|
| files indexed | 303 |
| contracts (build_index) | 2505 |
| contracts (extraction pass) | 2505 |
| E — extract_contracts total (avg of 5) | 14.551 ms |
| T_build — warm build_index | 131.3 ms |
| E / T_build | 11.09% (gate: < 15%) |

E covers both surfaces: per-language `extract_contracts` over the
grammar files plus the document scanners' `extract_document_contracts`
(see the per-document table). The grpc pre-pass tree walk runs only for
the seven languages with gRPC facts, so C/C++/Ruby/PHP/C# files pay no
walk at all.

Per-language extraction p50/p95 (last round):

| language | p50 | p95 | files |
|---|---|---|---|
| C | 13.2 us | 15.8 us | 25 |
| C# | 34.7 us | 56.4 us | 25 |
| C++ | 13.6 us | 15.6 us | 25 |
| Go | 54.0 us | 57.5 us | 25 |
| Java | 47.6 us | 70.6 us | 25 |
| JavaScript | 61.7 us | 77.2 us | 25 |
| PHP | 24.9 us | 27.7 us | 25 |
| Python | 55.7 us | 71.8 us | 25 |
| Ruby | 31.5 us | 34.5 us | 25 |
| Rust | 58.2 us | 100.9 us | 25 |
| TSX | 50.8 us | 103.1 us | 25 |
| TypeScript | 51.3 us | 56.0 us | 25 |

Per-document extraction p50 (last round; TASK-088 document path):

| document | kind | p50 | contracts |
|---|---|---|---|
| docs/openapi.yaml | OpenApi | 1474.0 us | 1000 |
| docs/records.json | OpenApi | 284.2 us | 0 |
| docs/schema.graphql | GraphQL | 32.7 us | 100 |
| docs/users.proto | Proto | 37.9 us | 30 |

Measurement conditions: the tables above were recorded while this shared
machine carried heavy external load (system load average 8-30 from other
users' jobs), which inflates every absolute number. In quiet windows
(load < 7) the same binary measured E 9.2-9.4 ms, T_build 66-70 ms, ratio
13.0-13.5%, and the no-walk languages recovered to at-or-below the base
636a4c5 p50s: C 8.5-8.7 us (base 10.5, pre-fix 17.1), C++ 8.7-8.8 (base
10.9, pre-fix 18.0), Ruby 20.4-20.5 (base 25.3, pre-fix 38.1), PHP 16.2
(base 20.0, pre-fix 31.9), C# 22.2 (base 27.7, pre-fix 41.7). The gate
passed in every quiet run; under peak load (avg 22+) single runs reached
15.0-15.3% purely from CPU contention.
