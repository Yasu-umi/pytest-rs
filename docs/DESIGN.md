# pytest-rs Design

## Goal

**Make pytest fast.** pytest-rs is a drop-in compatible re-implementation of pytest in Rust:
existing pytest test suites (`@pytest.fixture`, `conftest.py`, `pytest.mark.*`, `pytest.ini` /
`pyproject.toml`) run unchanged, but the runner — startup, collection, assertion rewriting,
coverage measurement, fixture orchestration, reporting — is Rust.

Test bodies themselves execute on embedded CPython (via pyo3). This is a hard floor: arbitrary
Python code with C extensions cannot run on anything else (RustPython is slower than CPython and
incompatible with C extensions). Throughput beyond single-core CPython comes from
**multi-process parallel execution** (pytest-xdist equivalent), implemented with a Rust
orchestrator. **Default is 1 process**; parallelism is opt-in via `-n N`.

Where the speed comes from:

| pytest cost | pytest-rs |
|---|---|
| Startup (importing pytest + plugins, hundreds of ms) | Rust binary, minimal Python shim |
| Collection (walk → import → introspect) | Rust dir walk + AST pre-scan filter (ruff parser) before import |
| Assertion rewriting (Python AST transform) | Rust AST transform, CPython only does `compile()` |
| coverage.py trace overhead (2–5x slowdown) | Native `sys.monitoring` callbacks in Rust with `DISABLE` dedup |
| Per-test orchestration (fixtures, reports) | Rust |
| xdist worker management (Python, heavy) | Rust process workers, cheap IPC |

## Decisions (fixed)

- **Drop-in pytest compatibility** is the target, not a new convention. The old `fix_` prefix POC is discarded.
- **Plugins are Rust crates** implementing a pluggy-like `Plugin` trait, compiled in via feature flags. Bundled: asyncio, mock, cov, split, benchmark. Third-party plugins (e.g. a future pytest-aiohttp) slot in the same way.
- **Coverage is Rust-native** via `sys.monitoring` (PEP 669, Python 3.12+) — coverage.py is not used.
- **Parser**: `ruff_python_parser` / `ruff_python_ast` (git-pinned; unpublished on crates.io). `rustpython-parser` is dropped.
- **pyo3**: latest. Workspace pins exactly one version; plugin crates use it via `pytest_rs_core::pyo3` re-export.
- **Multi-process parallelism prioritized** over free-threaded CPython / subinterpreters. Default 1 process.

## Architecture

### Crate layout

```
crates/
  pytest-rs-core/        # engine: config, collection, fixtures, runner, report,
                         #   hook traits + PluginManager, pyo3 boundary, embedded pytest shim
  pytest-rs-asyncio/     # event-loop lifecycle, async test/fixture execution (LoopRunner trait)
  pytest-rs-mock/        # `mocker` fixture (embedded Python shim wrapping unittest.mock)
  pytest-rs-cov/         # sys.monitoring native coverage, term/xml/lcov reports
  pytest-rs-split/       # --splits/--group, .test_durations
  pytest-rs-benchmark/   # `benchmark` fixture (#[pyclass]), calibration + stats
  pytest-rs/             # CLI binary: feature-gated plugin assembly, worker mode
```

### Core engine (`pytest-rs-core`)

```
src/
  config/      # pytest.ini / pyproject / setup.cfg + CLI (clap behind an OptionParser facade)
  collect/     # collection tree (arena), node IDs, AST pre-scan filter
  fixture/     # FixtureDef registry, dependency DAG resolution, scope cache, finalizer stack
  runner/      # setup/call/teardown protocol -> TestReport
  mark/        # marker model, -k / -m expression evaluator (small Pratt parser, no eval)
  assertion/   # Rust AST assert-rewrite -> regenerated source -> CPython compile()
  report/      # terminal (pytest-parity output), junitxml, exit codes 0-5
  python/      # the ONLY module allowed to touch Python<'py>/Bound. interp, shim loader,
               #   meta_path importer, introspection, traceback formatting
  hooks.rs     # Plugin trait, HookContext, RuntestGuard
  manager.rs   # PluginManager + Engine (disjoint borrows: plugins vs session)
  py/          # embedded `pytest` Python shim sources (include_str!)
```

Key structural rules:

1. **`'py` never escapes the `python/` boundary or a hook call.** Engine structs and plugin
   fields store GIL-independent `Py<PyAny>` handles, re-bound per GIL session. (The POC's
   `Executer<'a>` lifetime-threading does not scale.)
2. **The `pytest` module is ours.** Tests do `import pytest`, so the binary embeds a pure-Python
   shim package written to a per-run temp dir on `sys.path`, backed by a `#[pymodule]` native
   module (`append_to_inittab!`). Decorators **record metadata** (attach marker attributes
   exactly like real pytest); the Rust engine introspects after import. Names are never trusted.
3. **Import-based collection.** AST pre-scan is only a "should we import this file" fast path,
   never the source of truth (decorators are runtime constructs).
4. **Assertion rewriting**: meta_path finder (`pytest/_rewrite.py` in the shim) intercepts
   test-module loads and rewrites `assert` statements on the CPython ast
   (`ast.NodeTransformer` + `copy_location`, a simplified port of pytest's AssertionRewriter),
   then `compile()`s — line numbers stay exact. Rewriting happens once per module at import;
   a Rust (ruff) transform was considered but source-text regeneration loses location fidelity,
   so it stays a possible later optimization only if profiling shows import-time rewrite cost.
5. **Fixture engine** ports pytest's `SetupState` model: scope keys
   (function/class/module/package/session), autouse ordering, LIFO finalizer stack, fixture
   params expanding items at collection time. This is the hardest correctness area — golden-test
   teardown order against real pytest.

### Hook / plugin system

- **One `Plugin` trait**, dyn-compatible, default no-op method per hook, `Vec<Box<dyn Plugin>>`.
- Hooks take `&mut HookContext<'py, '_>` bundling `Python<'py>`, `&mut Session`, `&Config`.
- **firstresult** hooks return `PyResult<Option<T>>`; the manager stops at the first `Some`.
  The core owns the actual test call (`pytest_pyfunc_call` firstresult; asyncio claims async
  items). Same for `pytest_fixture_setup`.
- **hookwrapper** semantics via RAII guards: `around_runtest_call` returns
  `Option<Box<dyn RuntestGuard + 'py>>`; the manager runs guards LIFO after the call
  (benchmark timing, cov contexts).
- **Plugin-provided fixtures** two ways: `FixtureProvider::PySource` (embedded `@pytest.fixture`
  Python source — mocker) or `FixtureProvider::Native` (resolved via `pytest_fixture_setup` —
  benchmark's `#[pyclass]`).
- **Registration**: explicit feature-gated assembly in the binary (`#[cfg(feature = "...")]`),
  not `inventory`. `PluginManager::resolve_order()` topo-sorts `depends_on()` then stable-sorts
  per-hook `Order` (TryFirst/Normal/TryLast) into precomputed dispatch lists.
- **Inter-plugin coupling** only via `depends_on()` ordering + `Session::stash`
  (`HashMap<TypeId, Box<dyn Any>>` + well-known string keys, e.g. `"asyncio.event_loop"`).
  Never crate-to-crate Rust deps between plugins. This is how pytest-aiohttp plugs in later.

### Multi-process execution (`-n N`, default 1)

- `-n 0`/absent: everything in-process (no worker overhead at all).
- `-n N`: main process collects (fast: pre-scan + import once), then spawns N workers — the same
  binary in a hidden `--worker` mode. Work distribution over a simple length-prefixed
  JSON IPC on stdin/stdout (work-stealing queue of node IDs; module-affinity batching so a
  worker imports each test module once). Each worker embeds CPython, runs its items with the
  full plugin stack, streams `TestReport`s back; the parent merges reports, cov bitmaps
  (roaring merge is cheap), and durations.
- Session-scoped fixtures are per-worker (same semantics as xdist).
- Architecture consequence for everything else: collection, execution, and reporting communicate
  through serializable types (`TestItem`, `TestReport` are plain data) — never via shared Python
  state.

### Bundled plugins (v1 scope)

| Plugin | Mechanism | Hooks (main) |
|---|---|---|
| **asyncio** | `asyncio_mode auto/strict`, loop cache per `loop_scope`, `LoopRunner` trait (asyncio now; trio/uvloop later). Owns running coroutines: async tests via `pytest_pyfunc_call`, async (gen) fixtures via `pytest_fixture_setup` + finalizer driving `__anext__` | pyfunc_call, fixture_setup, collection_modifyitems, sessionfinish |
| **mock** | Embedded Python shim (`MockFixture` wrapping unittest.mock, patch bookkeeping + `stopall` on teardown). Fixtures: `mocker` + class/module/package/session variants. Thin Rust loader | configure (register fixtures) |
| **cov** | `sys.monitoring` tool id 1, LINE events, Rust `#[pyfunction]` callback returning `DISABLE` (each line costs one callback ever). Hits in `HashMap<file, RoaringBitmap>`. Denominator from ruff AST executable-line analysis + `exclude_lines` regexes. Reports: term/term-missing, Cobertura XML, lcov (HTML later; branch coverage deferred) | sessionstart/finish, terminal_summary (+ fail_under → exit code) |
| **split** | `.test_durations` JSON (nodeid → seconds), `--splits N --group K`, algorithms `duration_based_chunks` (order-preserving) / `least_duration` (LPT greedy), unknown tests get mean duration | addoption, collection_modifyitems (+ deselected), around_runtest_call (timing), sessionfinish |
| **benchmark** | `benchmark` fixture: `#[pyclass]` backed by Rust; inner loop is a generated tiny Python `for` driven once per round (one FFI crossing per round, parity with pytest-benchmark numbers); `perf_counter` clock; calibration vs clock resolution, warmup, stats (min/max/mean/stddev/median/iqr/outliers/ops). `--benchmark-json`, `--benchmark-only/skip`. pedantic mode | configure, collection_modifyitems, terminal_summary, sessionfinish |

## Milestones

- **M0 — Foundations**: workspace re-org (core + plugin crate skeletons + binary), POC deleted,
  pyo3 updated, ruff parser pinned, `python/interp.rs` (`Py<T>` handle store), shim
  embed + load, `Plugin` trait + `PluginManager`.
- **M1 — Minimal drop-in run** (single process): import-based collection with marker
  introspection, node IDs, function-scope fixtures (yield + async), `request`, asyncio plugin
  (pyfunc_call/fixture_setup), basic terminal report + exit codes.
  Target: `sample/` rewritten to standard pytest runs; a small real pytest suite passes.
- **M2 — Assertion rewriting**: meta_path hook, Rust rewriter, line-fidelity, rich failure output.
- **M3 — Fixture engine completeness**: all scopes + teardown ordering, autouse, conftest.py
  hierarchy + visibility, parametrize (test + fixture), `pytest.raises/approx/skip/xfail`.
- **M4 — Multi-process workers**: `-n N`, worker mode, IPC protocol, report/cov merge.
- **M5 — Config & selection parity**: ini/toml/addopts, `-k`/`-m`, `--lf/--ff` cache,
  `--collect-only`, `--tb` modes, junitxml, builtin fixtures (tmp_path, monkeypatch, capsys, caplog).
- **M6 — Plugins**: mock → cov → split → benchmark (asyncio already landed in M1).
  Order rationale: mock validates `register_fixture` with minimal surface; cov is the most
  isolated; benchmark composes everything so it goes last.

## Conformance testing

Compatibility is verified by running the **upstream test suites of the libraries being
reproduced** (pytest, pytest-asyncio, pytest-mock, pytest-cov, pytest-split, pytest-benchmark)
under pytest-rs, as-is.

- `conformance/` harness: pins each upstream repo at a known tag (checkout or sdist), runs its
  test suite with pytest-rs, and compares against an expected-results manifest
  (`conformance/<lib>/expected.toml`: pass / xfail-with-reason / excluded-with-reason).
- CI gate: the pass set may only grow; any newly-failing previously-passing upstream test fails CI.
- Exclusions are explicit and justified: tests that import library internals (`_pytest.*`,
  pluggy internals, pytest-rs doesn't have them) or test the plugin's packaging rather than
  behavior. Tests using the `pytester` fixture are a large category in pytest's own suite —
  supporting `pytester` (running pytest-rs as the sub-runner) is itself a milestone goal because
  it unlocks most upstream behavioral tests.
- Per-milestone targets: M1 picks a small curated subset (e.g. pytest's fixture/collection
  acceptance tests); each milestone expands the manifest rather than writing parallel
  hand-written compat tests.

## Risks

1. **Fixture scope/teardown ordering** — subtlest algorithm; port pytest's stack model, golden-test against real pytest.
2. **Assertion-rewrite line fidelity** — wrong line numbers poison every traceback; preserve line counts, snapshot-test failure output vs pytest.
3. **ruff crates unpublished** — git pin, isolate behind `assertion/` + `prescan` so a parser swap is contained.
4. **conftest/rootdir/import-mode semantics** — silent breakage of whole suites; implement `prepend` import mode first.
5. **cov accuracy vs coverage.py** — executable-statement set parity (multiline stmts, decorators, match); accept documented deltas in v1, branch coverage deferred.
6. **nodeid format parity** — split/cache/xdist all key on it; must match pytest exactly.
