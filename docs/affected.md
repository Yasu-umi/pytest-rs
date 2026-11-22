# `--affected`: run only the tests affected by your changes

Experimental. The design and its reasoning are in [AFFECTED_TESTS.md](AFFECTED_TESTS.md).

`--affected` records which code each test ran. On later runs it uses that record to run only the tests whose code, or the inputs they read, changed since then. Without an `--affected*` option pytest-rs behaves exactly as usual.

```sh
pytest-rs --affected          # select, run, and record what ran (the usual local loop)
pytest-rs --affected=record   # run as usual and record everything that ran
pytest-rs --affected=select   # select from the record, write nothing
```

The record lives in `.pytest-rs-affected` in the rootdir. `--affected-data=PATH` puts it elsewhere. Without a usable record (none yet, a different Python, a change in the installed packages or pytest-rs version), everything runs, and the header line says why.

`--affected` cannot be combined with `-n` yet.

## What runs

A test runs when any of these is true:

- code it ran has changed: a function it called, the class or module around it, or code run while its file or its fixtures were set up;
- a file it read or a directory it listed under the rootdir has changed;
- it is new, it failed last time, or it was named on the command line (`file.py::test_name`);
- it started a subprocess, whose own dependencies are not recorded (see `incomplete` below).

A subprocess started outside any test makes every test incomplete, so everything runs. This happens when a conftest or plugin starts one at import or at session start. The header line says so; `incomplete = "trust"` relies on the recorded dependencies instead.

A test file whose tests would all be skipped, and whose own import is unaffected, is not imported at all.

Edits that only touch formatting or comments, and newly added functions that no test called, run nothing. `-k`, `-m` and `--lf` still filter on top of the selection.

## Configuration

Configuration goes in `pyproject.toml`, under its own table:

```toml
[tool.pytest-rs.affected]
# Any change under these runs everything (adds to a built-in list: pytest.ini,
# pyproject.toml, tox.ini, setup.cfg, lock and requirements files, setup.py,
# native code and its sources, *.pth).
full_run_on = ["docker/**", ".python-version"]

# A changed file no test read and no rule matches: "ignore" (default) or "all".
unknown_files = "ignore"

# Treated as changed on every run: tests that depend on them always run.
always_changed = ["src/app/settings.py"]

# These tests always run.
always_run = ["tests/smoke/**"]

# Never looked at.
ignore = ["docs/**"]

# Tests that start a subprocess: "run" them always (default) or "trust" what was recorded.
incomplete = "run"

# A change under the key runs: a list of test globs, "recorded" (trust what tests
# were seen reading) or "all".
[tool.pytest-rs.affected.file_triggers]
"migrations/**" = ["tests/db/**"]
"data/**" = "recorded"
```

## Keep application imports out of the conftest's top level

If a `conftest.py` imports your application at module level, the code that the import runs counts toward every test. That code is the module-level statements and class bodies. Then any edit to a module constant or a class attribute anywhere in the application runs everything:

```python
# conftest.py: every test now depends on all of app's import-time code
from app import create_app

@pytest.fixture
def client():
    return create_app().test_client()
```

Import inside the fixtures that need it instead. The import then runs while the fixture is set up, and its code is attributed only to the tests that use the fixture:

```python
# conftest.py
@pytest.fixture
def client():
    from app import create_app
    return create_app().test_client()
```

The default deliberately runs too much rather than too little here. Some tests depend on module-level values without calling any function of that module, so dropping this rule would miss them.

## Known gaps

`--affected` cannot see:

- code that did not run in the recorded run;
- targets replaced by mocks;
- state outside the process (databases, environment variables, the clock);
- files read only by C libraries;
- changes inside installed packages, except as a full run when their versions change.

The full list is in [AFFECTED_TESTS.md §8](AFFECTED_TESTS.md#8-known-gaps-documented-and-accepted). Use `always_changed`, `always_run` or `file_triggers` for what it cannot see.
