# Bundled embedding benchmark

Measured 2026-07-29 with the checked-in
`potion-code-16M-v2` q4 artifact
(`399f26f790d90374b94a451374a8cbafc1fa6d2099cc6db60c9ae50e44e914c9`).

Command:

```sh
cargo bench --bench bundled_embedding
```

The benchmark generates and structurally indexes a deterministic Rust
repository containing exactly 10,000 functions before it starts the embedding
timer. The timed section runs in a fresh benchmark process and therefore
includes cold model decompression, validation, tokenizer initialization,
inference, and SQLite storage. It ran in the default restricted environment
without network access.

| Environment | Value |
|---|---:|
| CPU | Apple M4 |
| OS | macOS 26.3.1 (arm64) |
| Available/Rayon threads | 10 / 10 |
| Structural indexing (not timed) | 0.984 s |
| Cold embedding + storage | **0.605 s** |
| Throughput | **16,524.9 symbols/s** |
| Stored vectors | 10,000 (`bundled`, 256 dimensions) |
| Requirement | < 60 s |

Result: **PASS**, with 59.395 seconds of headroom on the measured machine.
