#!/usr/bin/env python3
"""Measure embedding retrieval quality on wonk's labeled semantic corpus.

The chunk JSONL files must be produced by:

    cargo run --release --example export_embedding_corpus -- /path/to/repo

This script intentionally keeps model acquisition out of CI. Candidate paths
are local Model2Vec directories and the Ollama baseline must already be pulled.
"""

from __future__ import annotations

import argparse
import concurrent.futures
import json
import urllib.request
from collections import defaultdict
from pathlib import Path
from typing import Callable

import numpy as np
from semantic_metrics import summarize


def load_chunks(path: Path) -> list[dict[str, object]]:
    with path.open(encoding="utf-8") as source:
        return [json.loads(line) for line in source if line.strip()]


def normalize(vectors: np.ndarray) -> np.ndarray:
    norms = np.linalg.norm(vectors, axis=1, keepdims=True)
    return vectors / np.maximum(norms, 1e-12)


def ollama_embed(
    texts: list[str], *, urls: list[str], model: str, batch_size: int
) -> np.ndarray:
    work = list(enumerate(range(0, len(texts), batch_size)))

    def embed_one(item: tuple[int, int]) -> tuple[int, np.ndarray]:
        batch_index, offset = item
        body = json.dumps(
            {"model": model, "input": texts[offset : offset + batch_size]}
        ).encode()
        request = urllib.request.Request(
            f"{urls[batch_index % len(urls)].rstrip('/')}/api/embed",
            data=body,
            headers={"Content-Type": "application/json"},
        )
        with urllib.request.urlopen(request, timeout=300) as response:
            payload = json.load(response)
        print(
            f"ollama: completed batch {batch_index + 1}/{len(work)}",
            flush=True,
        )
        return batch_index, np.asarray(payload["embeddings"], dtype=np.float32)

    with concurrent.futures.ThreadPoolExecutor(max_workers=len(urls)) as executor:
        completed = list(executor.map(embed_one, work))
    completed.sort(key=lambda item: item[0])
    return np.concatenate([batch for _, batch in completed])


def candidate_encoder(
    path: Path, quantization: str | None
) -> Callable[[list[str]], np.ndarray]:
    from model2vec import StaticModel

    model = StaticModel.from_pretrained(path)
    if quantization is not None:
        vectors = model.embedding.astype(np.float32)
        maximum = {"q4": 7.0, "q8": 127.0}[quantization]
        scale = np.max(np.abs(vectors), axis=1, keepdims=True) / maximum
        scale = np.maximum(scale, 1e-12)
        quantized = np.clip(np.rint(vectors / scale), -maximum, maximum)
        model = StaticModel(
            (quantized * scale).astype(np.float32),
            model.tokenizer,
            config=model.config,
            normalize=True,
        )

    def encode(texts: list[str]) -> np.ndarray:
        return np.asarray(
            model.encode(texts, use_multiprocessing=False), dtype=np.float32
        )

    return encode


def first_relevant_rank(
    rows: list[dict[str, object]], scores: np.ndarray, relevant: set[tuple[str, str]]
) -> int | None:
    order = np.argsort(-scores)
    for rank, index in enumerate(order, 1):
        row = rows[int(index)]
        if (str(row["file"]), str(row["symbol"])) in relevant:
            return rank
    return None



def evaluate(
    corpus: dict[str, object],
    chunks: dict[str, list[dict[str, object]]],
    encode: Callable[[list[str]], np.ndarray],
) -> dict[str, object]:
    doc_vectors = {
        repo: normalize(encode([str(row["text"]) for row in rows]))
        for repo, rows in chunks.items()
    }
    queries = list(corpus["queries"])
    query_vectors = normalize(encode([str(item["query"]) for item in queries]))
    ranks: list[int | None] = []
    by_repo: dict[str, list[int | None]] = defaultdict(list)
    details = []
    for item, query_vector in zip(queries, query_vectors):
        repo = str(item["repo"])
        relevant = {
            (str(target["file"]), str(target["symbol"]))
            for target in item["relevant"]
        }
        # Element-wise reduction avoids spurious overflow warnings emitted by
        # Accelerate's float32 matrix-vector path on large matrices.
        scores = np.sum(doc_vectors[repo] * query_vector, axis=1)
        rank = first_relevant_rank(chunks[repo], scores, relevant)
        ranks.append(rank)
        by_repo[repo].append(rank)
        details.append({"id": item["id"], "rank": rank})
    return {
        **summarize(ranks),
        "per_repo": {repo: summarize(repo_ranks) for repo, repo_ranks in by_repo.items()},
        "queries": details,
    }


def parse_candidate(value: str) -> tuple[str, Path, str | None]:
    name, separator, path = value.partition("=")
    if not separator:
        raise argparse.ArgumentTypeError("candidate must be NAME=PATH")
    quantization = next(
        (suffix for suffix in ("q4", "q8") if name.endswith(f"-{suffix}")),
        None,
    )
    return name, Path(path), quantization


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--corpus", type=Path, required=True)
    parser.add_argument("--chunks", type=Path, required=True)
    parser.add_argument("--candidate", action="append", default=[], type=parse_candidate)
    parser.add_argument("--ollama-url", action="append", default=[])
    parser.add_argument("--ollama-model", default="nomic-embed-text")
    parser.add_argument("--ollama-batch-size", type=int, default=32)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()

    corpus = json.loads(args.corpus.read_text(encoding="utf-8"))
    repos = sorted({str(item["repo"]) for item in corpus["queries"]})
    chunks = {repo: load_chunks(args.chunks / f"{repo}.jsonl") for repo in repos}
    available = {
        repo: {(str(row["file"]), str(row["symbol"])) for row in rows}
        for repo, rows in chunks.items()
    }
    for query in corpus["queries"]:
        missing = [
            target
            for target in query["relevant"]
            if (str(target["file"]), str(target["symbol"]))
            not in available[str(query["repo"])]
        ]
        if missing:
            raise SystemExit(f"{query['id']}: unresolved judgments: {missing}")

    results = {
        "schema_version": 2,
        "metric_semantics": "first_relevant_rank_only",
        "corpus_queries": len(corpus["queries"]),
        "repository_revisions": corpus["repositories"],
        "models": {},
    }
    for name, path, quantization in args.candidate:
        print(f"evaluating {name}")
        results["models"][name] = evaluate(
            corpus, chunks, candidate_encoder(path, quantization)
        )
    if args.ollama_url:
        print(f"evaluating {args.ollama_model}")

        def encode_ollama(texts: list[str]) -> np.ndarray:
            return ollama_embed(
                texts,
                urls=args.ollama_url,
                model=args.ollama_model,
                batch_size=args.ollama_batch_size,
            )

        results["models"][f"ollama:{args.ollama_model}"] = evaluate(
            corpus, chunks, encode_ollama
        )

    args.output.write_text(json.dumps(results, indent=2) + "\n", encoding="utf-8")


if __name__ == "__main__":
    main()
