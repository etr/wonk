#!/usr/bin/env python3
"""Deterministically quantize and pack wonk's bundled Model2Vec artifact."""

from __future__ import annotations

import argparse
import hashlib
import json
import struct
from pathlib import Path

import numpy as np
import zstandard
from safetensors.numpy import load_file

MAGIC = b"WNKEMB01"
VERSION = 1
HEADER = struct.Struct("<8s6I")


def sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def quantize_q4(vectors: np.ndarray) -> tuple[np.ndarray, np.ndarray]:
    vectors = vectors.astype(np.float32)
    scales = np.max(np.abs(vectors), axis=1) / 7.0
    scales = np.maximum(scales, np.finfo(np.float32).tiny).astype("<f4")
    quantized = np.clip(np.rint(vectors / scales[:, None]), -7, 7).astype(np.int8)
    return quantized, scales


def pack_nibbles(values: np.ndarray) -> bytes:
    flat = values.reshape(-1).astype(np.int8)
    if flat.size % 2:
        flat = np.append(flat, np.int8(0))
    unsigned = np.bitwise_and(flat.astype(np.int16), 0x0F).astype(np.uint8)
    return (unsigned[0::2] | (unsigned[1::2] << 4)).tobytes()


def build_container(config: bytes, tokenizer: bytes, vectors: np.ndarray) -> bytes:
    if vectors.ndim != 2:
        raise ValueError("embeddings tensor must be two-dimensional")
    rows, dim = vectors.shape
    quantized, scales = quantize_q4(vectors)
    weights = pack_nibbles(quantized)
    header = HEADER.pack(
        MAGIC,
        VERSION,
        rows,
        dim,
        len(config),
        len(tokenizer),
        len(weights),
    )
    return b"".join((header, config, tokenizer, scales.tobytes(), weights))


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--model", type=Path, required=True)
    parser.add_argument("--tokenizer", type=Path, required=True)
    parser.add_argument("--config", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--level", type=int, default=19)
    args = parser.parse_args()

    tensors = load_file(args.model)
    vectors = tensors.get("embeddings")
    if vectors is None:
        raise SystemExit("model does not contain an embeddings tensor")
    source_model = args.model.read_bytes()
    tokenizer = args.tokenizer.read_bytes()
    config = args.config.read_bytes()
    unpacked = build_container(config, tokenizer, vectors)
    compressor = zstandard.ZstdCompressor(
        level=args.level, threads=1, write_checksum=True, write_content_size=True
    )
    packed = compressor.compress(unpacked)
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_bytes(packed)
    print(
        json.dumps(
            {
                "format_version": VERSION,
                "rows": int(vectors.shape[0]),
                "dimension": int(vectors.shape[1]),
                "quantization": "symmetric-rowwise-q4",
                "source_model_sha256": sha256(source_model),
                "tokenizer_sha256": sha256(tokenizer),
                "config_sha256": sha256(config),
                "unpacked_bytes": len(unpacked),
                "packed_bytes": len(packed),
                "output_sha256": sha256(packed),
            },
            indent=2,
            sort_keys=True,
        )
    )


if __name__ == "__main__":
    main()
