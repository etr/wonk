# Bundled embedding model

Wonk bundles `minishlab/potion-code-16M-v2` at revision
`e9d2a44ca6a05ac6685f3b23709ea57eb7352d5b`. It is an MIT-licensed,
code-retrieval Model2Vec model with a 63,457-token vocabulary and 256 output
dimensions.

The checked-in artifact uses deterministic symmetric row-wise 4-bit
quantization followed by zstd level-19 compression. It is 6,518,611 bytes on
disk and expands to a 9,400,755-byte container at first use. The provider
validates the container and decompresses it once per process; it performs no
file reads or network requests.

On an Apple M4, cold model initialization, inference, and SQLite storage for
10,000 symbols took 0.605 seconds. The model selection, 25-query hit_rate@10,
and exact delta from the opt-in `nomic-embed-text` tier are recorded in
[`bench/semantic-quality.md`](../../bench/semantic-quality.md): both tiers
measured 0.20 hit_rate@10 (0.0 percentage-point delta), while Ollama had higher
MRR@10.

Regenerate from previously verified upstream files:

```sh
python3 -m venv /tmp/wonk-model-pack
/tmp/wonk-model-pack/bin/pip install numpy safetensors zstandard
/tmp/wonk-model-pack/bin/python scripts/pack_bundled_model.py \
  --model /path/to/model.safetensors \
  --tokenizer /path/to/tokenizer.json \
  --config /path/to/config.json \
  --output assets/models/bundled-embedding-v1.bin.zst
```

The command output must exactly match `manifest.json`. CI does not download or
regenerate the model.

Standalone GitHub release binaries embed these model weights. Each release
also includes `wonk-bundled-model-LICENSE.txt`, an exact copy of this directory's
MIT notice naming Minish Lab. Keep that notice with redistributed binaries.
