#!/usr/bin/env bash
set -euo pipefail

model_path="${1:-assets/models/bundled-embedding-v1.bin.zst}"
binary_path="${2:-}"
model_limit=$((10 * 1024 * 1024))
binary_limit=$((40 * 1024 * 1024))

check_size() {
  local label="$1"
  local path="$2"
  local limit="$3"
  local size

  if [[ ! -f "$path" ]]; then
    echo "ERROR: ${label} not found: ${path}" >&2
    return 1
  fi
  size=$(wc -c < "$path" | tr -d ' ')
  echo "${label}: ${size} bytes (limit ${limit})"
  if (( size > limit )); then
    echo "ERROR: ${label} exceeds its size budget" >&2
    return 1
  fi
}

check_size "Bundled embedding artifact" "$model_path" "$model_limit"
if [[ -n "$binary_path" ]]; then
  check_size "Stripped release binary" "$binary_path" "$binary_limit"
fi
