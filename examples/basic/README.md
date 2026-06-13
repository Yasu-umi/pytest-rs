# Basic example

A minimal Python project showing pytest-rs as a drop-in replacement for pytest.
No configuration changes needed — the same `pyproject.toml`, fixtures, and markers work as-is.

## Setup

```sh
pip install pytest-rs   # or: uv add --dev pytest-rs
```

## Run

```sh
pytest-rs                               # run all tests
pytest-rs -v                            # verbose output
pytest-rs -k "not slow"                 # filter by marker expression
pytest-rs -n 4                          # parallel workers (bundled)
pytest-rs --cov=my_project              # coverage (bundled)
pytest-rs --benchmark-only              # benchmarking (bundled)
pytest-rs -n 4 --cov=my_project -v      # combine freely
```

## What's here

```
tests/
├── conftest.py        — shared fixtures (tmp_path, custom)
├── test_basics.py     — assertions, parametrize, markers, pytest.raises
├── test_capture.py    — capsys, caplog, monkeypatch, tmp_path, custom markers
├── test_async.py      — async/await tests (pytest-asyncio bundled)
├── test_mock.py       — mocker fixture (pytest-mock bundled)
└── test_benchmark.py  — benchmark fixture (pytest-benchmark bundled)
```

All bundled plugins (`pytest-asyncio`, `pytest-mock`, `pytest-cov`, `pytest-benchmark`, `pytest-xdist`-style `-n`) work out of the box — no separate `pip install` needed.
