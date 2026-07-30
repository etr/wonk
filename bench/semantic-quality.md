# Semantic embedding quality bakeoff

Measured 2026-07-29 on an Apple M4 (macOS 26.3.1, arm64). The corpus has
25 labeled queries—five each from pinned revisions of ripgrep, Tokio, HTTPX,
Pydantic, and Fastify—and 59,138 candidate symbol chunks produced by wonk's
Rust chunker. A result is relevant only when both its repository-relative file
and symbol name match a checked-in judgment in `semantic_corpus.json`.

Recall is macro-averaged across queries. MRR and NDCG are truncated at rank 10.
Every model receives identical query text and chunk text; the Ollama row uses
the existing `nomic-embed-text` request path without task prefixes.

## Results

| Model | Revision / tier | Quantization | Dim | Source weights | Packed candidate | R@1 | R@5 | R@10 | MRR@10 | NDCG@10 |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| `potion-base-2M` | `389b9f64be5aa4ae7a6bc6fe95ef20ce485ae5da` | row-wise q8 | 64 | 7,559,256 B | 2,067,224 B | 0.00 | 0.04 | 0.04 | 0.0133 | 0.0200 |
| `potion-base-4M` | `9b3cff412d30be9ae8603fe10224c224f3401869` | row-wise q8 | 128 | 15,118,424 B | 3,781,818 B | 0.00 | 0.04 | 0.08 | 0.0167 | 0.0315 |
| **`potion-code-16M-v2`** | `e9d2a44ca6a05ac6685f3b23709ea57eb7352d5b` | **row-wise q4** | **256** | 32,490,072 B | **6,518,611 B** | **0.00** | **0.08** | **0.20** | **0.0480** | **0.0835** |
| `nomic-embed-text` | Ollama 0.30.8 baseline | upstream f16 | 768 | 274,290,656 B | external | 0.08 | 0.16 | 0.20 | 0.1080 | 0.1297 |

The selected model's per-repository Recall@10 was 0.40 on ripgrep, 0.20 on
Tokio, 0.20 on HTTPX, 0.00 on Pydantic, and 0.20 on Fastify.
The corresponding Ollama values were 0.20, 0.40, 0.20, 0.00, and 0.20.

The bundled model's Recall@10 delta versus `nomic-embed-text` was
**0.00 absolute, 0.0 percentage points, and 0% relative**. Ollama nevertheless
ranked relevant results earlier: its MRR@10 was 0.1080 versus 0.0480 and its
NDCG@10 was 0.1297 versus 0.0835. The bundled tier is intentionally the
zero-setup tier; Ollama remains available when this early-rank quality
difference is worth the external service and 274 MB model.

## Decision

`potion-code-16M-v2` q4 was selected. It achieved 0.20 Recall@10, 2.5 times the
4M control's recall, while remaining 3,967,149 bytes below the 10 MiB artifact
ceiling. It also had the best Recall@10 per packed MiB of the eligible
candidates. Static base models lost too much code retrieval recall; transformer
candidates were excluded at the pre-gate because no complete model,
tokenizer, and supported in-process Rust runtime fit the 10 MiB artifact
budget.

The checked-in artifact is MIT licensed, deterministic, and independently
covered by `assets/models/manifest.json`. Its cold 10,000-symbol inference and
SQLite storage benchmark took 0.605 seconds (16,524.9 symbols/s), and the
stripped release binary measured 36,647,776 bytes. These pass the 60-second
offline, 10 MiB artifact, and 40 MiB binary gates.

This measurement resolves OQ-009. It is deliberately a small, exact
symbol-retrieval corpus, so the absolute scores should be treated as a
regression baseline rather than a general-purpose model benchmark.

## Reproduction

Clone the revisions recorded in `semantic_corpus.json`, index them with wonk,
and export exact chunks:

```sh
cargo run --release --example export_embedding_corpus -- /path/to/repository \
  > /path/to/chunks/repository.jsonl
```

With the candidate Model2Vec directories present locally and
`nomic-embed-text` already pulled:

```sh
python bench/semantic_bakeoff.py \
  --corpus bench/semantic_corpus.json \
  --chunks /path/to/chunks \
  --candidate base2-q8=/path/to/potion-base-2M \
  --candidate base4-q8=/path/to/potion-base-4M \
  --candidate code16-q4=/path/to/potion-code-16M-v2 \
  --ollama-url http://127.0.0.1:11434 \
  --output /tmp/wonk-semantic-bakeoff.json
```

Model acquisition is intentionally outside this script and outside CI.
Ordinary builds and bundled inference never access the network.
