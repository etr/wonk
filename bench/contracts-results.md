# TASK-082 contract extraction build-cost results

Synthetic repo: 12 languages x 25 files, ordinary symbols plus 2-5
framework idioms per file. `cargo bench --bench contracts`.

E is measured serially over trees parsed exactly as `parse_one_file`
parses them (read/parse outside the timed window); T_build is the warm
`build_index` wall time, which parses and extracts in parallel. The gate
E < 0.15 x T_build is therefore conservative.

| metric | value |
|---|---|
| files indexed | 300 |
| contracts (build_index) | 1000 |
| contracts (extraction pass) | 1000 |
| E — extract_contracts total (avg of 5) | 6.483 ms |
| T_build — warm build_index | 66.6 ms |
| E / T_build | 9.73% (gate: < 15%) |

Per-language extraction p50/p95 (last round):

| language | p50 | p95 | files |
|---|---|---|---|
| C | 10.5 us | 11.4 us | 25 |
| C# | 27.3 us | 31.5 us | 25 |
| C++ | 10.8 us | 13.2 us | 25 |
| Go | 26.2 us | 28.5 us | 25 |
| Java | 20.1 us | 34.3 us | 25 |
| JavaScript | 28.3 us | 32.1 us | 25 |
| PHP | 20.0 us | 22.5 us | 25 |
| Python | 25.6 us | 28.5 us | 25 |
| Ruby | 16.1 us | 19.0 us | 25 |
| Rust | 23.8 us | 27.7 us | 25 |
| TSX | 19.1 us | 21.7 us | 25 |
| TypeScript | 23.5 us | 26.0 us | 25 |
