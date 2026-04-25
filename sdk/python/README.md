# C-Two Python SDK

This directory contains the published Python package for C-Two.

## Development

From the repository root:

```bash
uv sync
uv sync --reinstall-package c-two
C2_RELAY_ADDRESS= uv run pytest sdk/python/tests -q --timeout=30
```

Run Rust checks from the repository root:

```bash
cd core && cargo check --workspace
```

## Examples

Python examples live under `../../examples/python/`:

```bash
uv run python examples/python/local.py
```

## Benchmarks

Python-specific benchmarks live in `benchmarks/`:

```bash
C2_RELAY_ADDRESS= uv run python sdk/python/benchmarks/segment_size_benchmark.py
```
