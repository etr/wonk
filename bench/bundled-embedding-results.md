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

## Memory footprint (review debt, 2026-10-02)

The 6.5 MB zstd artifact expands well beyond its size once decoded
(quantified per the declared assumption; model2vec-rs 0.2.1
`StaticModel::from_bytes` materializes the i8 tensor as a dense owned
f32 matrix):

- Steady state, per process holding the model: ~62 MiB resident
  (63,457 rows x 256 dim x 4 B) — ~10x the 6.5 MB blob, ~8x the 9.4 MB
  unpacked artifact. The daemon holds it indefinitely; one-shot CLI
  processes hold it for their lifetime.
- Peak transient RSS during decode: ~97 MiB (unpacked artifact +
  safetensors section copies + serialized buffer + the f32 expansion,
  all live simultaneously), settling back to the ~62 MiB steady state.

Accepted cost for the offline no-dependency default. If the daemon's
footprint ever matters, the identified path is pooling directly over
the packed q4 rows with per-row scale applied at pool time (~16 MiB
matrix) or an f16 `from_borrowed` decode.
