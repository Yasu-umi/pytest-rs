# pytest-rs benchmarking & profiling

pytest-rs is a Rust binary with an **embedded CPython** (the shim layer in
`crates/pytest-rs-core/py/`). Slow spots can live on either side — the Rust
engine (collection, run loop, reporting) or the Python shim (assertion
rewrite, fixtures, capture). The tools here see both.

## 1. `bench.py` — attribute wall-clock with A/B variants

Times `--collect-only` (import + rewrite + collect, no DB needed) under several
configs so you can *attribute* cost instead of just totalling it:

| variant | what it isolates |
|---|---|
| `rs-collect` | pytest-rs normal |
| `rs-collect-plain` (`--assert=plain`) | rewrite OFF — `rs-collect − this` = **rewrite/compile cost** |
| `py-collect-cold` | real pytest, `__pycache__` cleared |
| `py-collect-warm` | real pytest, pyc cached — `cold − warm` = **the rewritten-.pyc cache pytest-rs forgoes** |

```sh
python bench/bench.py tests \
  --rs-bin target-py314/release/pytest-rs-bin \
  --cwd /path/to/suite \
  --venv /path/to/suite/.venv \
  --py /path/to/suite/.venv/bin/python \
  --reps 5
```

Why this design: pytest-rs **deliberately bypasses the bytecode cache**
(`_RewriteLoader.get_code`) because pytester rewrites files within the same
second+size, which CPython's mtime-based pyc validation misses. The cost of
that choice is exactly `py-cold − py-warm` on a warm dev loop. If `bench.py`
shows it's large, the fix is hash-based pyc (PEP 552), which is invalidation-
correct under same-second rewrites *and* caches.

## 2. Flame graph — `flame.sh` (py-spy `--native`)

One graph with both Python shim frames and native Rust frames.

**Build a symbol-bearing binary** (release strips symbols):

```sh
CARGO_TARGET_DIR=target-prof PYO3_PYTHON=/opt/homebrew/opt/python@3.14/bin/python3.14 \
  RUSTFLAGS="-C force-frame-pointers=yes" cargo build --profile profiling
# -> target-prof/profiling/pytest-rs-bin
```

(The `[profile.profiling]` in `Cargo.toml` = release speed + debug symbols.)

**Record** (macOS py-spy needs sudo to sample a process):

```sh
bench/flame.sh /tmp/rs.svg /path/to/suite/.venv -- \
  target-prof/profiling/pytest-rs-bin tests --collect-only -q
open /tmp/rs.svg
```

## 3. samply — native deep-dive, no sudo

For Rust-only hotspots with an interactive Firefox-Profiler UI (Python frames
show as CPython C internals, so prefer py-spy for shim questions):

```sh
cargo install samply
CARGO_TARGET_DIR=target-prof ... cargo build --profile profiling
samply record -- target-prof/profiling/pytest-rs-bin tests --collect-only -q
```

## 4. `suites.sh` — end-to-end suite benchmarks (the README perf table)

Reproduces the numbers in the README **Performance** table by cloning real
open-source suites at pinned tags, setting up a venv, and timing real pytest
vs pytest-rs (median of N, real/rs interleaved so the ratio survives background
load). The checkouts are disposable — no submodules; clone, measure, delete.

```sh
bench/suites.sh target/release/pytest-rs-bin /tmp/perf            # all suites
bench/suites.sh target/release/pytest-rs-bin /tmp/perf click      # one suite
rm -rf /tmp/perf                                                  # discard
```

| suite | tag | `--cov` target | parallel mode | test deps |
|---|---|---|---|---|
| marshmallow | 4.1.1 | `marshmallow` | `-n 3` (xdist) | `simplejson==4.1.1` |
| click | 8.3.1 | `click` | `-n 3` (xdist) | — |
| pydantic | v2.13.4 | `pydantic` | `--parallel-threads 3` | `hypothesis`, `pytest-run-parallel` |

Gotchas baked into the script (why pinned/why these flags):

- **Pin the versions, don't track latest.** The *latest* marshmallow/click
  releases don't run clean out of the box — marshmallow's suite imports
  `simplejson` (a test-only dep), and click's latest has collection errors —
  so a naive run measures a near-empty/error run, not real work. The pinned
  tags match the conformance submodules and pass under pytest-rs at 100%.
- **pydantic is parallelized with `pytest-run-parallel`, not xdist.** Its
  Makefile runs `pytest --parallel-threads N`; plain `pytest -n 3` (xdist)
  fails on pydantic with a "different tests collected" error. pytest-rs loads
  `pytest-run-parallel` too, so both runners are measured under
  `--parallel-threads 3` for an apples-to-apples parallel comparison.
- **Real baseline is pytest 9.0.3** (the version pytest-rs reproduces) for
  marshmallow/click; pydantic keeps its own pinned pytest from `uv sync`.
- Coverage is the interesting axis: pytest-rs uses `sys.monitoring`, real
  pytest uses `coverage.py`'s trace hooks, so the gap is widest on `--cov`.

## Build/run constraints (see project memory)

- The conformance runner uses `target/debug`; **don't `cargo build` (debug) while it runs**. Profiling builds go to a *separate* `CARGO_TARGET_DIR` (`target-prof`), which is safe to run in parallel.
- darwin debug build: `PYO3_PYTHON=/opt/homebrew/opt/python@3.13/bin/python3.13 cargo build`.
- To run pytest-rs against another project's venv, pass that venv via `VIRTUAL_ENV` and use a binary built against the matching Python minor version.
