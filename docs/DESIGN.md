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
- **Plugins are Rust crates** implementing a pluggy-like `Plugin` trait, compiled in via feature flags. Bundled: asyncio, anyio, mock, cov, split, benchmark. Third-party plugins (e.g. a future pytest-aiohttp) slot in the same way.
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
  pytest-rs-anyio/       # anyio-marked tests/fixtures via the installed anyio's TestRunner
  pytest-rs-mock/        # `mocker` fixture (embedded Python shim wrapping unittest.mock)
  pytest-rs-cov/         # sys.monitoring native coverage, term/xml/lcov reports
  pytest-rs-split/       # --splits/--group, .test_durations
  pytest-rs-benchmark/   # `benchmark` fixture (#[pyclass]), calibration + stats
  pytest-rs/             # CLI binary: feature-gated plugin assembly, worker mode
```

### Core engine (`pytest-rs-core`)

```
src/
  config.rs    # pytest.ini / pyproject / setup.cfg + CLI (clap behind an OptionParser facade)
  collect.rs   # collection: dir walk, AST pre-scan filter (ruff), node IDs, TestItem list
  fixture.rs   # FixtureDef registry, dependency resolution, scope cache, finalizer stack
  request.rs   # FixtureRequest
  engine/      # Engine driver: run/run_session, collection orchestration (collect.rs),
               #   -k/-m selection, in-process nested runs (inprocess.rs), terminal
               #   rendering (terminal.rs), collector-tree collectstart/collectreport (hooks.rs)
  runner/      # setup/call/teardown protocol -> TestReport (item, marks, teardown, protocol)
  report.rs    # TestReport / CollectReport model, exit codes 0-5
  python/      # the ONLY modules allowed to touch Python<'py>/Bound: interp bootstrap,
               #   shim loader, collection, fixtures, reporter, proxies, services, tracebacks
  hooks.rs     # Plugin trait, HookContext, PluginManager
  dist.rs      # work distribution over -n workers (--dist modes, work-stealing queue)
  worker.rs    # hidden --worker mode (fork/spawn)
  ipc.rs       # newline-delimited JSON IPC between controller and workers
  cache.rs     # --lf/--ff lastfailed persistence
  session.rs   # Session state (items, collect errors, shouldstop/shouldfail)
  tw.rs        # terminal writer (color, width)
py/            # embedded `pytest` + `_pytest` Python shim packages (build.rs embeds them at
               #   compile time; written to a per-run temp dir on sys.path at startup)
```

(Marker model and the `-k`/`-m` evaluator live partly in the shim — `pytest._marks`,
`pytest._expression` — and partly in `engine/selection.rs`; assertion rewriting is a
Python meta-path transform in the shim's `pytest/_rewrite.py`, not a Rust module.)

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
- **hookwrapper** semantics for *python* hooks landed with pytest-timeout support:
  `pytest_runtest_protocol` (around the whole item) and `pytest_runtest_call` (around the
  call phase) generator hookimpls run pluggy-style — pre-yield part before the phase,
  post-yield after, LIFO unwind. Other py hook names still drive generators to completion
  in place. Native (Rust) plugins haven't needed wrapper semantics: benchmark times inside
  its fixture and split reads `TestReport.duration`.
- **pluggy-lite shim pluginmanager** (`pytest._pluginmanager`): autoloaded plugin modules and
  conftests register into it; `config.pluginmanager.hook.<name>(**kw)` dispatches custom
  hooks (kwarg-filtered, LIFO, `firstresult` honored via `pytest_addhooks` +
  `add_hookspecs`). `pytest_addoption(parser)` fires against a parser shim
  (`pytest._parser`) that records option/ini specs: `config.getoption()/getini()` fall back
  to plugin-declared defaults (typed, e.g. bool inis), and unknown `--flag[=value]` CLI
  tokens deferred at clap time resolve against those specs after plugin load (unregistered
  leftovers usage-error, pytest parity). `config.stash` (`pytest.Stash/StashKey`),
  `node.config`, `item.session.config/shouldfail`, `pytest_report_header`, and `--markers`
  round out the plugin-facing surface (pytest-timeout runs fully: signal + thread methods,
  marker/ini/CLI config, session timeout, custom-hook overrides from conftest).
- **Terminal-reporter replacement** (pytest-sugar/pytest-pretty): a default
  `TerminalReporter` (trimmed port of upstream's, `_pytest.terminal`) registers as the
  `terminalreporter` plugin before python `pytest_configure` fires. A plugin that
  unregisters it and registers its own subclass flips the engine into *delegated mode*
  (`Config::reporter_delegated`): every native terminal print is suppressed (the
  `no_terminal()` gate) and `pytest._reporter` drives the replacement object through the
  hook calls upstream pluggy would make — `pytest_sessionstart` (it owns the header; the
  native header only prints when output stays native), `pytest_collection_finish`
  ("collected N items"), `pytest_deselected`, `pytest_runtest_logstart/logreport/logfinish`
  per item (after conftest impls, pluggy LIFO), `pytest_collectreport` for collection
  errors, and the end-of-run summary sequence (`summary_errors → summary_failures →
  warnings → passes → other plugins' pytest_terminal_summary → short_test_summary →
  summary_stats`, upstream's sessionfinish-wrapper order). The report proxy's string
  longrepr grows upstream's `.reprcrash`/`.chain` surface so crash-message suffixes and
  pretty's failure table work. Native runs pay nothing: the default reporter is inert and
  the engine renders in Rust. `-n` runs feed the controller-side reporter in arrival order
  (xdist behavior); forked workers drop the inherited replacement (stdout is the IPC pipe).
  pytest-pretty's output is byte-identical to real pytest 9.0.3 on the mixed-outcome demo;
  sugar needs a tty (or `--force-sugar`), upstream behavior. Not delegated: --collect-only
  trees and --cache-show stay native.
- **`pytest_collection_preexpand`** (pytest-rs-specific hook): runs after collection but
  before parametrized-fixture expansion, so plugins can inject closure-affecting marks —
  anyio's usefixtures("anyio_backend") injection lands here, making the backend a normal
  outermost fixture-param axis exactly like upstream's makeitem-time injection.
- **Plugin-provided fixtures** two ways (both landed in M6):
  - *PySource*: the plugin ships an embedded Python package written into the per-run shim dir
    and registered via `python::register_plugin_fixtures` — mock's `pytest_mock` package.
    Writing real files (not `PyModule::from_code`) keeps normal import machinery working
    (`pytest_mock._util` submodule imports) and lets the assertion rewriter process the shim
    (`pytest.register_assert_rewrite("pytest_mock")`, like pytest rewrites entry-point plugins).
  - *Native*: the plugin registers a raising stub `@pytest.fixture` for name resolution and
    claims the actual setup in `pytest_fixture_setup` — benchmark's `#[pyclass]` fixture.
- **Registration**: explicit feature-gated assembly in the binary (`#[cfg(feature = "...")]`),
  not `inventory`; plugins run in registration order (`depends_on()`/`Order` topo-sort is
  deferred until ordering actually matters). At runtime `-p no:NAME` (also `no:pytest_NAME`)
  drops a bundled plugin before the engine starts, matching pytest semantics.
- **Inter-plugin coupling** only via `depends_on()` ordering + `Session::stash`
  (`HashMap<TypeId, Box<dyn Any>>` + well-known string keys, e.g. `"asyncio.event_loop"`).
  Never crate-to-crate Rust deps between plugins. This is how pytest-aiohttp plugs in later.
- **Entry-point autoload** (pytest's setuptools plugin loading): installed `pytest11` entry
  points import under the shim and register their module-level fixtures and `pytest_*`
  hooks — pure fixture-provider plugins (Faker, requests-mock, respx, time-machine,
  pytest-aiohttp) work as-is. Distributions pytest-rs bundles natively (pytest-asyncio,
  -mock, -cov, -split, -benchmark, -xdist) are skipped: their upstream modules target real
  pytest internals. anyio is deliberately *not* skipped: its plugin module's fixtures
  (anyio_backend & friends) register through autoload, while the hooks pytest-rs cannot
  emulate from Python live in the native anyio crate. `PYTEST_DISABLE_PLUGIN_AUTOLOAD` and `-p no:NAME` (entry-point or
  module name) opt out. Divergence: a plugin that fails to import warns
  (PytestConfigWarning) and is skipped instead of aborting the run — plugins built against
  pluggy/`_pytest` internals would otherwise make every run on that venv unusable. Loaded
  in the controller before conftests and mirrored in spawned workers (forked workers
  inherit). Autoloaded modules also register into the shim pluginmanager, so plugins
  shipping their own hookspecs (pytest-timeout's `pytest_timeout_set_timer`) dispatch and
  are overridable from conftests.

### Multi-process execution (`-n N`, default 1) *(landed in M4)*

- `-n 0`/absent: everything in-process (no worker overhead at all). `-n auto`/`-n logical`
  resolve like upstream xdist: `PYTEST_XDIST_AUTO_NUM_WORKERS` env override, then conftest
  `pytest_xdist_auto_num_workers` hooks (firstresult), then psutil if installed (the
  `pytest-xdist[psutil]` extra — physical cores for auto, logical for logical), then
  sched_getaffinity/cpu_count. `--maxprocesses` caps auto/logical only. The
  `[setproctitle]` extra also works: workers retitle per item ("[pytest-xdist running]
  nodeid" / idle), import-probed like upstream — extras need no special wiring, they are
  plain installed packages the embedded interpreter feature-detects the same way xdist does.
- `-n N`: main process collects (import once, applies -k/-m/modifyitems), then starts N
  workers. On unix the workers **fork off the already-imported parent** (after collection,
  before any thread exists): imported test modules, conftests, and the fixture registry
  arrive copy-on-write, so workers skip the per-process import cost upstream xdist pays.
  Forked children reseed `random`/`numpy.random` (fork duplicates PRNG state) and clear
  inherited collection-time warnings (the parent reports those). The parent sets
  `PYTEST_XDIST_WORKER*` through `os.environ` right before each fork and restores after,
  so the child holds its identity from its first instruction — visible to
  `os.register_at_fork` callbacks. `PYTEST_RS_DIST_SPAWN=1` opts back into
  spawn-per-worker; non-unix always
  spawns — the same binary in a hidden `--worker` mode, which imports every test module up
  front (xdist's collection phase, so test side effects cannot leak into module import
  time). Work distribution over newline-delimited JSON IPC: parent→worker on a clean
  stdin; worker→parent on stdout with a sentinel frame prefix (tests print via Python, so
  worker `sys.stdout` is aliased to stderr and stray fd-1 output passes through
  unmangled). Workers stream `TestReport`s back per phase.
- Dispatch granularity follows `--dist` (xdist parity): per-test for `load`/`worksteal`
  (default), per-module for `loadscope`/`loadfile`/`loadgroup` (each module imported by one
  worker), duplicated per worker for `each`. The queue is work-stealing: idle workers wait on
  a condvar while batches are in flight (a crash may requeue work).
- Crash handling: a dead worker fails its running test (`worker gwN crashed while running …`),
  requeues the rest, and is replaced while `--max-worker-restart`'s budget lasts; an exhausted
  budget aborts undispatched work and banners `xdist: maximum crashed workers reached: N`.
  Crash bookkeeping is atomic under the queue lock, so concurrent crashes resolve
  deterministically: one of them exhausts the budget; crashes landing after the abort are
  silent (their tests count as undispatched, not failed). Replacements always spawn
  (re-forking is unsafe once the owner threads exist), so a replaced worker pays the full
  import cost — fine for the rare crash path.
- Merging: reports stream into the parent in arrival order; plugins serialize per-process
  state through `pytest_worker_dump`/`pytest_worker_load` (cov hits as JSON line sets —
  roaring stayed unnecessary at this scale — benchmark results as stats JSON); worker warnings
  forward into the parent's summary; split durations come from the merged reports for free.
- Session-scoped fixtures are per-worker (same semantics as xdist). The `worker_id` /
  `testrun_uid` fixtures and `PYTEST_XDIST_WORKER*` env vars are provided for compatibility,
  plus an `xdist` import shim (`is_xdist_worker` etc.) and `config.workerinput`.
- Known divergence: the parent imports test modules during collection (xdist collects in
  workers), so module-level side effects run once in the parent too. Under fork mode this
  cuts deeper: module-level code that reads `PYTEST_XDIST_WORKER` at import time captured
  the parent's (unset) value — every worker would see the same snapshot. Such suites must
  read worker identity lazily (the `worker_id` fixture, or env reads inside fixtures) or
  run with `PYTEST_RS_DIST_SPAWN=1`.
- Architecture consequence for everything else: collection, execution, and reporting communicate
  through serializable types (node IDs in, `TestReport`s out) — never via shared Python state.

### Bundled plugins (v1 scope)

| Plugin | Mechanism | Hooks (main) |
|---|---|---|
| **asyncio** | `asyncio_mode auto/strict`, loop cache per `loop_scope`, `LoopRunner` trait (asyncio now; trio/uvloop later). Owns running coroutines: async tests via `pytest_pyfunc_call`, async (gen) fixtures via `pytest_fixture_setup` + finalizer driving `__anext__` | pyfunc_call, fixture_setup, collection_modifyitems, sessionfinish |
| **anyio** | `anyio_mode auto/strict` (strict default). The real anyio dist stays entry-point autoloaded (anyio_backend & friends register as plugin fixtures, incl. user conftest overrides); the crate ports only the runner glue from `anyio.pytest_plugin` (lease-counted `get_runner`, backend `TestRunner`s — asyncio, asyncio+uvloop and trio all work). anyio-marked coroutine tests get a usefixtures("anyio_backend") mark injected in `pytest_collection_preexpand` (function-level, so the backend expands as the outermost fixture-param axis with upstream-identical IDs and ordering); a clone-per-backend fallback remains in collection_modifyitems. The backend value reaches hooks via fixture kwargs → callspec → engine fixture cache → raw param. Async (gen) fixtures run per backend (`pytest_fixture_cache_key` suffix); asyncgen fixtures hold their runner lease open setup→teardown, so a module-scoped one shares its loop with the module's tests, like upstream | pyfunc_call, fixture_setup, fixture_cache_key, collection_preexpand, collection_modifyitems |
| **mock** | Adapted upstream pytest-mock shim shipped as a real `pytest_mock` package in the shim dir (assert-rewritten, so `assert_called_*` introspection diffs match pytest). Fixtures: `mocker` + class/module/package/session variants; `stopall` via generator-fixture teardown; assert-method traceback wrapping (`mock_traceback_monkeypatch`) | configure (write package, wrap asserts, register fixtures), sessionfinish (unwrap) |
| **cov** | `sys.monitoring` tool id 1 (COVERAGE_ID), LINE events, Rust `#[pyclass]` callback returning `DISABLE` (each line costs one callback ever). Hits in `HashMap<file, BTreeSet<u32>>` (roaring deferred to M4 merge work). Denominator from ruff AST executable-line analysis + `exclude_lines` regexes (.coveragerc / --cov-config / pyproject; default `# pragma: no cover`); observed-but-unanalyzed lines union into the denominator. Reports: term/term-missing (+skip-covered), Cobertura XML, lcov (HTML/JSON later; branch coverage deferred). `--cov-fail-under` forces exit code 1 | configure (start monitoring), sessionfinish (stop, build report, fail_under), terminal_summary |
| **split** | `.test_durations` JSON (nodeid → seconds, legacy list format accepted), `--splits N --group K`, algorithms `duration_based_chunks` (order-preserving) / `least_duration` (LPT greedy), unknown tests get mean duration of the relevant cached set; `--store-durations` aggregates `TestReport.duration` per nodeid | addoption, configure (validation), collection_modifyitems, sessionfinish (store) |
| **benchmark** | `benchmark` fixture: `#[pyclass]` backed by Rust; inner loop is a generated tiny Python `for` driven once per round (one FFI crossing per round, parity with pytest-benchmark numbers); `perf_counter` clock; calibration vs clock resolution, warmup, stats (min/max/mean/stddev/median/iqr/outliers/ops, `benchmark.stats.stats.min` API). `--benchmark-json`, `--benchmark-only/skip/disable`. pedantic mode with upstream call-count/validation parity (storage/compare/histogram/cprofile not reproduced) | addoption, configure, collection_modifyitems, fixture_setup (native claim), terminal_summary, sessionfinish (json) |

### Environment variables

All pytest-rs-specific env vars are namespaced `PYTEST_RS_*`. Most are set by
pytest-rs itself to wire up subprocesses/workers and are **not** meant to be
set by users; a small number are knobs users or the conformance harness may
set. (pytest's own `PYTEST_ADDOPTS`, `PYTEST_DEBUG_TEMPROOT`, etc. behave as
upstream and are not listed here.)

**User / harness knobs** (safe to set):

| Var | Effect |
|---|---|
| `PYTEST_RS_DISABLE_PLUGINS` | Comma/space-separated native plugins to disable, matched like `-p no:NAME` (bare or `pytest_`/`pytest-` prefixed). Unlike `-p no:`, it survives into nested `pytester` subprocess runs (which strip `PYTEST_ADDOPTS` and don't inherit the outer CLI's args), so it's how the conformance harness isolates an always-on native plugin out of an unrelated suite. |
| `PYTEST_RS_INLINE_INPROCESS` | Make `pytester`'s `runpytest`/`inline_run` execute in-process instead of spawning a subprocess. Used in conformance runs where the subprocess path masks failures. |
| `PYTEST_RS_DIST_SPAWN` | Opt `-n` workers back into spawn-per-worker (a fresh process each) instead of the default fork-based workers. |

**Internal plumbing** (set by pytest-rs; do not set by hand):

| Var | Role |
|---|---|
| `PYTEST_RS_EXE` | Path the embedded interpreter reports as `sys.executable` / uses to re-exec workers (the binary embeds libpython, so `sys.executable` would otherwise be wrong). |
| `PYTEST_RS_WORKERINPUT` | JSON `workerinput` handed to an `-n` worker (worker id, testrun uid, conftest-populated `configure_node` data). |
| `PYTEST_RS_HOOK_RELAY` / `PYTEST_RS_LOG_RELAY` | Socket paths a subprocess uses to relay hook calls / log records back to the controller. |
| `PYTEST_RS_FORWARDED_FILTERS` | Newline-separated warning filters forwarded from controller to child so captured warnings match. |
| `PYTEST_RS_COV_*` (`CHILD`, `ACTIVE`, `PATHS`, `OUT`, `SOURCES`, `BRANCH`, `ROOT`, `SIGTERM`, `TOOL_ID`) | Coverage child-process wiring: tells a spawned/forked process to start `sys.monitoring` coverage and where/how to dump its hits for the parent to merge. |

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
- **M4 — Multi-process workers** *(done)*: `-n N`, worker mode, IPC protocol, report/cov
  merge, crashed-worker replacement, `--dist` modes. Upstream pytest-xdist acceptance tests:
  60/102 at landing (execnet/DSession/looponfail internals excluded); enabling -n also lifted
  pytest-cov's suite (xdist-variant tests) from 28 to 46 passing.
- **M5 — Config & selection parity**: ini/toml/addopts, `-k`/`-m`, `--lf/--ff` cache,
  `--collect-only`, `--tb` modes, junitxml, builtin fixtures (tmp_path, monkeypatch, capsys, caplog).
- **M6 — Plugins** *(done)*: mock → cov → split → benchmark (asyncio already landed in M1).
  Order rationale: mock validates plugin-provided fixtures with minimal surface; cov is the most
  isolated; benchmark composes everything so it goes last. Landing M6 also pulled core parity
  work the upstream suites depended on: `pytest_generate_tests` (metafunc), pytest's rootdir
  algorithm (common ancestor of cwd + path-like args), `-k`/`-m` selection expressions,
  builtin `pytestconfig`/`tmpdir`/`testdir`/`capsys` fixtures, pytester `inline_run`,
  `==`-failure diff explanations (`_compare_eq_*`), and `-p no:NAME` plugin disabling.
  Upstream-suite scores at landing: pytest-mock 89/90 (1 env skip), pytest-split 59/59
  (3 internal-API files excluded), pytest-benchmark test_normal/test_sample green
  (storage/cli internals excluded), pytest-cov 28/209 (the rest is branch coverage, xdist,
  and html/json reports — deferred by design).
- **Post-v1 — conformance-driven hardening** *(ongoing)*: work since M6 is steered by running
  ever more of the upstream suites. Landmark pieces: entry-point autoload of third-party
  `pytest11` plugins (16 plugin suites now run their own tests under pytest-rs — pytest-mypy,
  -ruff, -subtests, -snapshot, -bdd, …); an in-process `pytester` backend (live `HookRecorder`
  hook-call monitoring, `getitem`/`getmodulecol` returning real collector nodes, nested-run
  global-state isolation); the collector-tree `pytest_collect_file`/`collect_directory`/
  `collectstart`/`collectreport` hook surface; `--pyargs`, `--junitxml` in nested runs; and a
  growing set of real-world suites (httpx, starlette, fastapi, werkzeug, pandas, scikit-learn)
  as drop-in evidence.

## Conformance testing

Compatibility is verified by running the **upstream test suites of the libraries being
reproduced** under pytest-rs, as-is, in three categories:

- *pytest & plugin ecosystem* (the APIs pytest-rs reimplements): pytest itself,
  pytest-asyncio, pytest-mock, pytest-cov, pytest-xdist, pytest-split, pytest-benchmark,
  and anyio.
- *Third-party plugins* (not reimplemented — loaded as-is through the entry-point shim,
  their own suites run under pytest-rs): pytest-timeout, pytest-mypy, pytest-ruff,
  pytest-subtests, pytest-metadata, pytest-snapshot, pytest-icdiff, pytest-socket,
  pytest-order, pytest-repeat, pytest-instafail, pytest-env, pytest-rerunfailures,
  pytest-randomly, pytest-bdd, and a partial pytest-django.
- *Real-world projects* (drop-in evidence — their suites run unchanged): click, jinja,
  marshmallow, rich, attrs, more-itertools, packaging, httpx, starlette, fastapi,
  werkzeug, pandas, and scikit-learn (sharded). Many pass at or near 100%; see
  `conformance/RESULTS.md` for the live per-suite numbers.

Harness (`conformance/runner.py`):

- `conformance/suites.toml` pins each upstream repo at a release tag; the runner clones the
  tag (or uses the submodule checkout with `--local`), installs suite deps into a `--target`
  dir (dist-info included, so a suite's own `pytest11` entry point autoloads — how anyio and
  pytest-timeout exercise the autoload path), and runs file by file. File selection honors the
  suite's own `python_files` ini (e.g. pytest collects `testing/python/*.py`), so the
  scoreboard measures exactly what upstream collects rather than only `test_*.py`.
- Results land in `conformance/scoreboard/<platform>/*.json` (linux = canonical, CI
  bot-refreshed on main pushes; darwin = dev) and regenerate `conformance/RESULTS.md` plus
  the README table.
- CI gate: `conformance/expected/<name>.toml` pins files that must keep passing — the list
  only grows; a regression fails CI. `[excluded]` entries are explicit and justified (tests
  of pytest/pluggy internals, packaging tests). The release gate additionally requires
  ci-green and a drift-free scoreboard.
- `pytester` is supported (nested sessions run the pytest-rs binary as the sub-runner),
  which is what unlocked the bulk of upstream behavioral tests. Its in-process APIs largely
  landed too (`parseconfig(ure)`, `runitem`, `getnode`/`getitems`/`collect_by_name`, a real
  `HookRecorder`). `getitem`/`getmodulecol` return live collector nodes — `Module`/`Class`/
  `Function` carrying `.obj`, a faithful `reportinfo()`, and a `Session.perform_collect()` that
  round-trips — so collector-introspection tests work. The main remaining gap is `spawn_pytest`
  (pexpect-driven interactive sessions, e.g. `--pdb` debugger tests).

## Risks

1. **Fixture scope/teardown ordering** — subtlest algorithm; port pytest's stack model, golden-test against real pytest.
2. **Assertion-rewrite line fidelity** — wrong line numbers poison every traceback; preserve line counts, snapshot-test failure output vs pytest.
3. **ruff crates unpublished** — git pin, isolate behind `assertion/` + `prescan` so a parser swap is contained.
4. **conftest/rootdir/import-mode semantics** — silent breakage of whole suites; implement `prepend` import mode first.
5. **cov accuracy vs coverage.py** — executable-statement set parity (multiline stmts, decorators, match); accept documented deltas in v1, branch coverage deferred.
6. **nodeid format parity** — split/cache/xdist all key on it; must match pytest exactly.
