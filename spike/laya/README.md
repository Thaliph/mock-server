# Spike #580: Laya as the mimic guard model

Scratch branch. This is not production code. The report is the comment on issue #580.

## Files

- `dataset.py`: the synthetic labelled set (85 samples, 8 categories). Fake secrets are random. Run it
  to make `dataset.jsonl`; the file is not committed.
- `flows.py`: 20 TCP and UDP flows as text (flow metadata and the first 64 bytes).
- `patterns.py`: the deterministic `pattern` rules (the baseline).
- `guards.py`: the model adapters. They refuse `pickle` and `torch.load` for the whole process.
- `evaluate.py`: precision and recall per question at the thresholds 0.3, 0.5 and 0.8.
- `bench.py`: cold start, RSS, service time, and the queue at 1, 50 and 200 decisions per second.
- `results/`: scores (ids and numbers only, no sample text), reports, hashes, package versions.

## Pins

| Artefact | Revision | Main file sha256 |
|---|---|---|
| `convaiinnovations/laya` (root, English) | `246177e5cb70334c9831c0730864a93c836aac8f` | `model.safetensors` `891102d372688fc2a094dac56a384bc537b87c63f21f9f3dac0be2b7cbc8d86c` |
| `impacte/bunker-laya` | `fe23fc44e9d2418371ac7ac5ec848121027a3e84` | `onnx/model.onnx` `949c055e…b893`, `onnx/model.onnx.data` `3311200689a7…1100` |
| `protectai/deberta-v3-base-prompt-injection-v2` | `90c9989b1a342275dd0d1a95aad283c04e075671` | `onnx/model.onnx` `f0ea7f239f765aed…228c` |
| `laya` (PyPI, installed with `--no-deps`) | 0.5.4 (tag `v0.5.4`) | see `results/python-packages.txt` |

Full hashes: `results/sha256.txt`. Python 3.12.16, `torch` 2.9.1+cpu, `onnxruntime` 1.32.0.

## How to run

Downloads go to separate empty directories outside this tree. Python runs with `-I`.

```sh
python3 -I dataset.py > "$DATA/dataset.jsonl"
python3 -I flows.py > "$DATA/flows.jsonl"
taskset -c 0,1 "$VENV/bin/python" -I evaluate.py laya-onnx "$DL/bunker" "$DATA/dataset.jsonl" results/bunker-laya
SPIKE_CALLS=10 taskset -c 0,1 "$VENV/bin/python" -I bench.py laya-onnx "$DL/bunker" "$DATA/dataset.jsonl" results/bench-bunker-laya.json
```

Notes:

- The run of `laya` (root) on the set had six questions per call: the five of `guards.QUESTIONS` and `flow`.
- `patterns.py` was written after the author saw the set. Its scores are an upper bound.
- `bench.py` measures the service time in a closed loop. The latency at 1, 50 and 200 decisions per
  second comes from a single FIFO worker that replays the measured service times (Lindley recursion).
- Machine: Intel Core i7-8750H at 2.20 GHz, 12 threads, 31 GiB. Each run used `taskset -c 0,1` and
  two ONNX Runtime or torch threads. The benchmarks started with a 1-minute load under 8
  (`results/bench-load.log`).
