# Changelog

Notable changes per release. The release workflow uses the matching section
as the GitHub release notes (auto-generated notes are the fallback).

## v0.0.6 (2026-06-21)

### Added

- **Fixture request object on statically collected items** — `TopRequest`
  now exposes faithful `getfixturevalue` / `_fillfixtures` / `addfinalizer`
  / `applymarker`, scope-gated `request.cls` / `request.function`, and
  `request.fixturenames` reflecting the running item's scope-sorted closure
  (including dynamically requested fixtures). Pytester-collected items
  resolve their fixtures in-process.
- **Dynamic fixture scope** — `@pytest.fixture(scope=<callable>)` is
  evaluated at resolution time (#1781).
- **`--fixtures` / `--fixtures-per-test` / `--funcargs`**, with colored
  output and conftest-aware headers.
- **`pytest_fixture_setup` / `pytest_fixture_post_finalizer` hooks** fire
  for conftest plugins (most-specific baseid first).
- **Fixtures from plugin instances** registered during `pytest_configure`
  are now discovered (#2270).
- Collection honors **`python_classes` / `python_functions`** ini options
  and custom nodes from `pytest_pycollect_makeitem` / `makemodule`.
- **`--fulltrace`** in traceback rendering; `Pytester.getitem` / `getitems`
  accept a `Path`.

### Changed

- **Coverage statement counting aligned with coverage.py** (#1) — file
  discovery skips dotted/special-character names, `...`-only stub bodies and
  module/class docstrings are excluded, bare constant expressions are
  counted, statement-free files report 0 statements, and bare annotations
  are counted scope-aware (PEP 563/649). Verified to match coverage.py 7.x
  statement-for-statement on large real-world projects.
- **Fixtures set up in scope-sorted closure order** (true execution order),
  with the closure following override chains and honoring
  parametrize-ignored arguments; super-fixture parametrization propagates
  through override-reuse.
- Class methods are collected in definition order across the MRO.
- `pytest-rerunfailures` runs in-process (now 48/48) and `pytest-subtests`
  improves to 28/32 (unittest subtest skip location and SUBFAIL message).

### Fixed

- **Fixture scope errors** — `ScopeMismatch` (including via
  `request.getfixturevalue` from a fixture) and invalid scope values are
  detected and reported like upstream; fixture cache keys are scope-qualified
  so class teardown no longer evicts module fixtures.
- **Fixture setup exceptions are cached** so they aren't re-run per item,
  and finalizer teardown errors are grouped into a `BaseExceptionGroup`.
- `getfixturevalue(<own-name>)` resolves to the overridden super fixture;
  `getfixturevalue` of a parametrized fixture without a bound param is
  reported clearly; double-yield fixtures fail like `fail_fixturefunc`.
- Rich repr for fixture-not-found (full request chain) and recursive
  dependency errors; double `@pytest.fixture` and a fixture named `request`
  are rejected at collection.
- `@pytest.fixture`-decorated functions are skipped in xunit setup/teardown
  lookup; higher-scoped autouse fixtures run before xunit setup hooks.
- `-s` / `--capture=no` output is collected into inline `runpytest` results;
  import errors and import-file-mismatch surface from live collector nodes.
- A fixture discovery no longer errors on an evil `__getattr__` (#214); the
  `pluggy` import in `_pytest._code` is optional.

## v0.0.5 (2026-06-16)

### Added

- **Live collector-tree nodes** — `pytester.getitem` / `getmodulecol` now
  return real `Module` / `Class` / `Function` collectors: the module node
  carries `.obj`, `Function` / `Class` expose a faithful `reportinfo()`
  (0-based lineno + modpath), `getmodulecol().session.perform_collect()`
  round-trips, and `Function.originalname` / `callobj` / `<Function name>`
  repr work. The real `Class` / `Function` / `Module` are re-exported from
  `_pytest.python` so upstream `isinstance` checks pass.
- **`--pyargs`** collection arguments (`pkg.mod::Test::case`), including
  namespace-package resolution.
- **`--junitxml` in nested (pytester) runs**, so upstream's `test_junitxml`
  suite runs in-process.
- **Ported internals for pytest's own tests**: `IdMaker` (parametrization ID
  derivation), `getfuncargnames` / `num_mock_patch_args` / `ascii_escaped`
  (`_pytest.compat`), `_recursive_sequence_map` (`_pytest.python_api`), and
  an `_pytest.raises` shim (`RaisesExc` / `RaisesGroup` / `repr_callable`).
- The bundled pytester plugin registers its `pytester_example_path` marker.

### Changed

- **Conformance fidelity** — the runner now honors each suite's
  `python_files` ini. pytest's own `testing/python/*.py` (`collect.py`,
  `fixtures.py`, `metafunc.py`, `raises.py`, `approx.py`, `integration.py`,
  …) were silently unmeasured before; they are now collected like upstream
  does. This adds previously-invisible tests to the denominator, so the
  pytest headline conformance number drops while becoming honest.

### Fixed

- `pytest_collect_file` fires for every file (with `repr_failure` on
  collection errors), `pytest_collect_file` skips are honored, and a
  `pytest_collect_directory` hook can filter/skip directories.
- Nested in-process (`pytester`) runs isolate more global state: `--basetemp`
  validation, the `--runxfail` monkeypatch, warning-capture state, the
  plugin-manager registry, and `pytest.exit()` in `sessionfinish` /
  `UsageError` written to fd 2 so `result.stderr` captures them.
- Terminal / assertion / traceback fidelity: file paths shown relative to the
  invocation dir when rootdir differs, `saferepr` in the assertion fallback,
  multi-line `reprcrash.message` matching, no spurious blank line before the
  first traceback frame, and `--color` / `--showlocals` / fine-grained
  verbosity propagated into nested runs.
- `runpytest_inprocess` returns a `RunResult` (not a `HookRecorder`).

### Conformance

- pytest's own suite is now measured faithfully at **2231 / 2833** graded
  tests after including `testing/python/*.py`; the previously reported number
  excluded those files. `python/collect.py` 42 → 35 failures and
  `python/metafunc.py` 71 → 49 from the live-node and `IdMaker` work.

## v0.0.4 (2026-06-11)

### Added

- **Custom-collector subsystem** — `pytest_collect_file` hooks returning
  `File` / `Item` subclasses (via `Node.from_parent`) are surfaced through
  the plugin manager into `session.items`. This unlocks collector-based
  plugins: `pytest-mypy`, `pytest-ruff`, `pytest-snapshot`, `pytest-icdiff`,
  and friends now run their own suites.
- **A large in-process pytester surface**, so upstream's own
  pytester-driven tests run without a real in-process pytest:
  `Pytester.parseconfig` / `parseconfigure`, `runitem`, `getnode` /
  `getitems` / `collect_by_name` / `getmodpath`, a real `HookRecorder`
  (PluginManager hook-call monitoring), `SetupState`, `ExceptionInfo` /
  `CallInfo`, `SysModulesSnapshot` / `SysPathsSnapshot`, and `LineMatcher`
  match-logging.
- **15 new conformance suites** for popular plugins/frameworks:
  `pytest-mypy`, `pytest-ruff`, `pytest-subtests`, `pytest-metadata`,
  `pytest-snapshot`, `pytest-icdiff`, `pytest-socket`, `pytest-order`,
  `pytest-repeat`, `pytest-instafail`, `pytest-env`, `pytest-rerunfailures`,
  `pytest-randomly`, `pytest-bdd`, and a partial `pytest-django`.
- **Config subsystem** — `Parser.parse_known_args`, `parseconfig` fires
  conftest/plugin `pytest_addoption` over an ini type system, `--strict-config`
  + unknown-config-option validation, fine-grained `--verbosity` and verbosity
  inis, `required_plugins`, `confcutdir`, the `pythonpath` ini, `pytest.toml` /
  `.pytest.ini` discovery, and `[pytest]`-section detection in `.cfg` files.
- **Terminal reporting** — `console_output_style`
  (progress/count/classic/times), `XFAILURES` / `XPASSES` sections and
  `--xfail-tb`, `--showlocals` / `-l`, captured teardown sections in
  failures/passes, verbose skip/xfail reasons, and header testpaths /
  `--no-header`.
- **`--dist=loadscope` / `loadfile` / `loadgroup` reorder work units by
  descending size** (xdist's default), gated by `--loadscope-reorder` /
  `--no-loadscope-reorder`.
- `setup_function` / `teardown_function`, plugin/conftest
  `pytest_generate_tests` + indirect parametrize, `pytest_assertrepr_compare`
  plugin hooks, a `threading.excepthook` capture plugin, and
  `pytest_load_initial_conftests` + the `PYTEST_PLUGINS` env var.

### Fixed

- A `UsageError` raised in `pytest_configure` exits with code 4 and still
  runs `pytest_unconfigure`; `Skipped` raised from `pytest_ignore_collect` /
  `pytest_collect_file` hooks is handled rather than crashing.
- `--stepwise` now passes its full upstream suite (18/18), with an
  `INTERRUPTED` exit when the session sets `shouldstop`.
- `--assert=plain` disables the rewrite finder, and a non-string user
  message in a rewritten assert is formatted (not stringified raw).
- Symlinked test paths preserve the symlink name only for file symlinks,
  not directory symlinks (matching pytest's collection).
- The subprocess pytester restores the dynamic-loader path (and import path)
  across `mock.patch.dict(os.environ, clear=True)` so nested runs still start.
- A `pyproject.toml` `addopts` written as a TOML array (e.g.
  `["--strict-markers", "-ra"]`) is split into separate arguments instead of
  collapsing into one bogus token.
- A linelist ini given as a TOML array (e.g. `markers`) still merges a plugin's
  `config.addinivalue_line` appends, so `--strict-markers` recognizes
  plugin-registered marks (e.g. pytest-django's `django_db`).
- `pytester.inline_genitems` returns real `DoctestItem`s (with a
  `DoctestModule` / `DoctestTextfile` parent) for `--doctest-modules` and
  `--doctest-glob` collection.

### Performance

- Rewritten bytecode is cached as a hash-checked `.pyc` and GC is disabled
  during collection — warm collection of a large suite drops from ~40s to
  ~22.8s. Adds a profiling harness (A/B collect timing + py-spy `--native`
  flame graphs) under `bench/`.

### Conformance

- pytest's own suite: 1421 → 1885 of 2246 graded tests, spanning config,
  terminal reporting, collection, the in-process pytester API, and custom
  collectors.
- New plugin suites scored on first landing (e.g. `pytest-snapshot` 100/107,
  `pytest-socket` 59/65, `pytest-order` 85/134); `pytest-django` is partially
  integrated (Django framework support is ongoing).

### Internal

- `_pytester.py` (1936 lines) split into focused `_pytester_config`,
  `_pytester_linematcher`, and `_pytester_relay` modules.

## v0.0.3 (2026-06-08)

### Added

- **Terminal-reporter replacement** — `pytest-sugar` and `pytest-pretty`
  take over the output as-is (the engine suppresses its native rendering
  and drives the registered replacement through the same hooks pluggy
  would); their output is byte-diffed against real pytest in CI.
- **More third-party plugins work through the `pytest` shim**, gated by a
  functional smoke test: `pytest-timeout`, `inline-snapshot`,
  `pytest-run-parallel` (`--parallel-threads` really runs each test on N
  threads), plus `anyio`'s own plugin. The pluggy-lite hook relay now
  implements wrapper/hookwrapper semantics, and `pytest_addoption` hooks
  receive the `pluginmanager` argument.
- **`anyio`** runs through a native runner plugin (per-backend
  parametrization, async fixtures over asyncio/trio); its upstream suite
  is part of conformance.
- **pytest-xdist data exchange**: `pytest_configure_node` /
  `config.workerinput` / `config.workeroutput` / `pytest_testnodedown`,
  `--dist=loadgroup` (xdist_group batching), and `-x`/`--maxfail` under
  `-n`. Distributed conformance 67% → 92%.
- **`--ignore` / `--ignore-glob`** prune paths during collection
  (fnmatch with character classes), and `--rootdir` is validated.
- `PYTEST_ADDOPTS` is honored (between ini `addopts` and the command line).

### Fixed

- **`--stepwise` was entirely inert** — the flag never reached the runner,
  so it never stopped after the first failure. `--stepwise`,
  `--stepwise-skip`, and `--stepwise-reset` now work.
- **`--keep-duplicates`** doubles a directory passed twice, not just a
  file passed twice.
- `@pytest.mark.parametrize(ids=callable)` runs the idfn per value
  (matching pytest's `_idval`), no longer raising spurious duplicate-ID
  collection errors under strict id checking.
- `session.shouldfail` / `session.shouldstop` are sticky — once set they
  cannot be unset (pytest issue #11706).
- `_pytest.assertion.rewrite.AssertionRewritingHook` is importable and
  usable as an explicit loader; a module docstring containing
  `PYTEST_DONT_REWRITE` opts out of rewriting.
- Rewritten asserts no longer keep their compared values alive in the
  frame — leak tests using `weakref` + `gc.collect()` pass, and
  GC-retry-loop suites run dramatically faster.
- `-k` / `-m` expressions use a faithful port of pytest's expression
  parser (kwargs syntax, exact error messages); `pytest.raises`
  `ExceptionInfo` gains the upstream `repr` and `match()` honors
  PEP-678 `__notes__`.
- Inside a virtualenv, `sys.executable` points at the venv interpreter, so
  tests spawning subprocesses through it see the venv's packages.

### Coverage

- pytest-cov's upstream suite: 105 → 182 of 209 graded tests. `--cov-append`
  with branch coverage round-trips internal arcs through a sidecar file,
  `--cov-precision` / `[report]` precision/show_missing/skip_covered are
  honored, and `--cov`'s subprocess hook installs into the active
  virtualenv's site-packages too.

### Conformance

- pytest's own suite: ~1180 → 1421 of 2193 graded tests, across
  `unittest` lifecycle, `tmpdir`/`unraisable`/`deprecated`, marks,
  skipping, sessions, and terminal output.
- **Real-world drop-in**: `pydantic`'s full 11,545-test suite passes
  (byte-identical collection), and `click` / `jinja` / `marshmallow` /
  `rich` run unchanged as conformance gallery suites.

## v0.0.2 (2026-06-06)

### Fixed

- **macOS wheels work outside the build machine.** The engine now links a
  python-build-standalone CPython, so the recorded libpython reference
  keeps the plain `libpython3.X` leaf name the launcher resolves via the
  loader path. The v0.0.1 macOS wheels only worked on machines with a
  framework Python at `/Library/Frameworks` — use v0.0.2 instead.
- `@pytest.mark.parametrize` accepts the `argnames=` / `argvalues=` keyword
  spelling (collection raised IndexError before).
- `pytest.importorskip` failures skip at module level instead of erroring
  collection.
- Fixtures requested via `@pytest.mark.usefixtures` now parametrize the
  test like signature fixtures do (`request.param` was missing).
- The `norecursedirs` ini option is honored during collection (pytest's
  defaults, including `fnmatch` path patterns).

### Coverage

- **Branch coverage**: `--cov-branch` and `[run] branch = true` measure
  branch arcs natively (sys.monitoring BRANCH events; per-direction on
  CPython 3.14, dis-classified on 3.13), with Branch/BrPart report columns
  and `13->15` / `11->exit` missing annotations matching coverage.py.
- **Subprocess coverage**: python child processes measure themselves
  through an env-gated site hook (a pure-python port of the native
  collector, no coverage.py dependency) and their results merge into the
  session report — `subprocess.run(...)`-spawned scripts now show up, like
  pytest-cov's process startup hook.
- Multi-line statements fold onto their first line, matching coverage.py's
  statement counting.
- `[paths]` aliasing rewrites measured paths to their canonical form in
  reports.
- pytest-cov's upstream suite: 47 -> 105 of 209 graded tests since v0.0.1.

### Release pipeline

- Publishing now requires the tagged commit's CI to be green and the
  committed conformance results to match a fresh run, and asserts the
  built wheels reference libpython by its plain leaf name.
- Release binaries are stripped and thin-LTO'd (wheel 2.9 MB -> 2.7 MB).

## v0.0.1 (2026-06-06)

First public release: a drop-in compatible pytest runner written in Rust,
focused on speed.

### Runner

- Import-based collection (`test_*.py` / `*_test.py`, `Test*` classes,
  `conftest.py`, `python_files` / `norecursedirs` ini patterns)
- Fixtures: function/class/module/package/session scopes, autouse, generator
  teardown, dependencies, `request` surface, parametrized fixtures
- `@pytest.mark.parametrize` (stacked marks, `pytest.param` ids/marks,
  keyword spelling), skip/skipif/xfail, custom marks with `-m` selection,
  `--strict-markers`
- Assertion rewriting with pytest-style failure explanations: diffs for
  strings/sequences/sets/dicts/dataclasses, `-v`/`-vv` verbosity levels,
  runtime truncation (`truncation_limit_lines` / `truncation_limit_chars`)
- Output capture (`fd` / `sys` / `tee-sys` / `-s`), per-phase captured
  sections on reports, `capsys` / `capfd` / binary variants
- logging integration: `caplog`, `log_cli` live logging, `--log-file`
- pytest-compatible terminal output, exit codes, `-q`/`-v`, `--tb` styles,
  `-x`/`--maxfail`, `--lf`/`--ff`/`--nf` (cacheprovider), `--stepwise`,
  `--collect-only`, `--junitxml`, `--basetemp`, warnings summary and
  `filterwarnings`
- conftest plugin hooks (`pytest_runtest_logreport`, report proxies),
  third-party plugins via the `pytest11` entry point and the `pytest` shim

### Bundled plugin compatibility

- pytest-asyncio (strict/auto), pytest-mock, pytest-cov (native line
  collector), pytest-split, pytest-benchmark, and `-n` parallel runs
  (fork-based workers on unix, xdist-style scheduling)
- Any bundled plugin can be disabled per run (`-p no:NAME`) or excluded at
  build time (Cargo features)

### Conformance

- The upstream test suites of pytest 9.0.3, pytest-asyncio, pytest-mock,
  pytest-cov, pytest-xdist, pytest-split and pytest-benchmark run unchanged
  under pytest-rs, scored per file and gated in CI — current numbers in
  [conformance/RESULTS.md](https://github.com/Yasu-umi/pytest-rs/blob/main/conformance/RESULTS.md)

### Distribution

- Prebuilt wheels for linux x86_64/aarch64 (manylinux_2_28) and macOS arm64,
  CPython 3.13/3.14, plus an sdist; ~2.7 MB with all plugins included
- The `pytest-rs` command is a small launcher that resolves the active
  environment's libpython at startup and execs the engine

### Known limitations

- unix only (no Windows)
- no `--pdb` / debugger integration yet
- requires a CPython with a shared libpython (uv / python.org / Homebrew /
  conda / distro builds are fine; plain pyenv builds need
  `PYTHON_CONFIGURE_OPTS="--enable-shared"`)
