"""First-relevant-rank measures; no model or numerical dependencies."""
from __future__ import annotations
import math
from typing import Iterable

def summarize(ranks: Iterable[int | None]) -> dict[str, float]:
    values = list(ranks)
    total = len(values)
    metrics = {
        f"hit_rate@{cutoff}": sum(rank is not None and rank <= cutoff for rank in values) / total
        for cutoff in (1, 5, 10)
    }
    metrics["mrr@10"] = sum(1.0 / rank for rank in values if rank is not None and rank <= 10) / total
    metrics["first_hit_discount@10"] = sum(1.0 / math.log2(rank + 1) for rank in values if rank is not None and rank <= 10) / total
    return metrics
