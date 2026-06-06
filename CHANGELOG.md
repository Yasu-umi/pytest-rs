# Changelog

Notable changes per release. The release workflow uses the matching section
as the GitHub release notes (auto-generated notes are the fallback).

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
