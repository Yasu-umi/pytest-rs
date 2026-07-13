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
open-source suites at pinned tags and timing real pytest vs pytest-rs (median of
N, real/rs interleaved so the ratio survives background load). Three modes per
suite: plain (no coverage), `--cov`, and the suite's parallel `--cov`. The
checkouts are disposable — no submodules; clone, measure, delete.

```sh
bench/suites.sh /tmp/perf                  # all suites, pytest-rs from PyPI
bench/suites.sh /tmp/perf click            # one suite
RS_SPEC=. bench/suites.sh /tmp/perf click  # measure a local dev build instead
rm -rf /tmp/perf                           # discard
```

pytest-rs is **installed into each suite's venv** (the way a user runs it), so it
self-locates and measures correctly — no external binary path, no `PYTHONHOME`,
portable across machines and CI. `RS_SPEC` is what to `uv pip install` (default
`pytest-rs` from PyPI; set to a repo path/wheel for a local build). `PYVER`
(default 3.13) and `REPS` (default 5) round out the knobs.

To add a suite, append one line to `spec()`
(`repo|tag|cov-source-path|parallel-flag|extra-deps|testpath`, `testpath`
optional, defaults to `tests`); it automatically gets all three modes. Pick
suites with **several thousand fast unit tests** — that's where the framework
layer is a real fraction of the wall clock. Avoid suites whose runtime is
dominated by slow test bodies (property-based fuzzing, DB, network): the
native engine can't speed up what runs inside a test.

| suite | tag | `--cov` source path | parallel mode | test deps |
|---|---|---|---|---|
| marshmallow | 4.1.1 | `src/marshmallow` | `-n 3` (xdist) | `simplejson==4.1.1` |
| click | 8.3.1 | `src/click` | `-n 3` (xdist) | — |
| networkx | networkx-3.6.1 | `networkx` | `-n 3` (xdist) | `numpy==2.4.6 scipy==1.18.0 pandas==3.0.3` |

marshmallow/click are mid-sized suites of fast unit tests, where the framework
layer (startup, collection, coverage, parallel) is a real fraction of the wall
clock even in plain mode. networkx is a large suite (6890 tests, numeric-heavy)
where plain mode is dominated by test-body cost (~1.0x) but `--cov`/`-n` show
pytest-rs's biggest wins — its tests live scattered under `networkx/**/tests/`,
so `testpath` is set to the whole `networkx` package rather than a single
`tests/` dir. Suites dominated by slow test bodies without that contrast make
poor benchmarks: e.g. pydantic's full suite is `hypothesis` property-test bound
(~40 s of fuzzing), so both runners land at ~1.0x — the native engine can't
speed up what runs inside a test body.

Gotchas baked into the script (why pinned / why these targets):

- **Pin the versions, don't track latest.** The *latest* marshmallow/click
  releases don't run clean out of the box — marshmallow's suite imports
  `simplejson` (a test-only dep), and click's latest has collection errors —
  so a naive run measures a near-empty/error run, not real work. The pinned
  tags match the conformance submodules and pass under pytest-rs at 100%.
- **Give `--cov` the in-tree source path, not the package name.** The suites are
  installed editable, so their source stays at `src/<pkg>`, and both runners
  measure those files. Passing the path keeps the comparison robust across
  pytest-rs versions; the script sanity-checks that pytest-rs reports a non-zero
  statement count and skips the cov rows otherwise.
- **Real baseline is pytest 9.0.3** — the version pytest-rs reproduces.
- Coverage is the interesting axis: pytest-rs uses `sys.monitoring`, real
  pytest uses `coverage.py`'s trace hooks, so the gap is widest on `--cov`.

## Build/run constraints (see project memory)

- The conformance runner uses `target/debug`; **don't `cargo build` (debug) while it runs**. Profiling builds go to a *separate* `CARGO_TARGET_DIR` (`target-prof`), which is safe to run in parallel.
- darwin debug build: `PYO3_PYTHON=/opt/homebrew/opt/python@3.13/bin/python3.13 cargo build`.
- To run pytest-rs against another project's venv, pass that venv via `VIRTUAL_ENV` and use a binary built against the matching Python minor version.
