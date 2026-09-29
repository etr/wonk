# Contract extraction build-cost results (TASK-082 + TASK-087)

Synthetic repo: 12 languages x 25 files, ordinary symbols plus 2-5
framework idioms per file (HTTP/env from TASK-082; ~2 message-kind
idioms — queue/websocket/job — per file from TASK-087).
`cargo bench --bench contracts`.

| metric | value |
|---|---|
| files indexed | 300 |
| contracts (build_index) | 1375 |
| contracts (extraction pass) | 1375 |
| E — extract_contracts total (avg of 5) | 8.570 ms |
| T_build — warm build_index | 83.8 ms |
| E / T_build | 10.23% (gate: < 15%) |

Per-language extraction p50/p95 (last round):

| language | p50 | p95 | files |
|---|---|---|---|
| C | 11.3 us | 12.2 us | 25 |
| C# | 27.9 us | 32.2 us | 25 |
| C++ | 11.5 us | 13.2 us | 25 |
| Go | 33.1 us | 63.8 us | 25 |
| Java | 26.0 us | 121.5 us | 25 |
| JavaScript | 37.2 us | 41.8 us | 25 |
| PHP | 20.1 us | 24.8 us | 25 |
| Python | 29.8 us | 36.2 us | 25 |
| Ruby | 23.8 us | 32.7 us | 25 |
| Rust | 36.1 us | 41.6 us | 25 |
| TSX | 26.1 us | 29.4 us | 25 |
| TypeScript | 34.2 us | 39.3 us | 25 |
