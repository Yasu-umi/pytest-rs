# pytest-rs

pytest-rs is a re-implementation of the popular Python testing framework [pytest](https://github.com/pytest-dev/pytest) in Rust, focused on **speed**: a drop-in compatible runner where startup, collection, fixture orchestration, coverage measurement, and reporting are native code, while test bodies run on embedded CPython.

> Note: This project is currently in active development (alpha stage). Many features are still under implementation and subject to change. See [docs/DESIGN.md](https://github.com/Yasu-umi/pytest-rs/blob/main/docs/DESIGN.md) for the architecture and roadmap.
>
> pytest-rs is an independent project, not affiliated with or endorsed by the pytest project.

## Installation

Prebuilt wheels are published to PyPI for Linux (x86_64 / aarch64) and macOS (arm64) on CPython 3.13 / 3.14:

```sh
uv add --dev pytest-rs    # or: pip install pytest-rs
```

Then run your existing suite, no changes needed:

```sh
pytest-rs                       # whole suite, like `pytest`
pytest-rs tests/test_foo.py     # one file
pytest-rs -n 4                  # parallel workers (pytest-xdist compatible)
pytest-rs --cov=mypkg           # native coverage (pytest-cov compatible)
```

`pytest-rs` reads the same configuration pytest does (`pytest.ini`, `pyproject.toml` `[tool.pytest]` / `[tool.pytest.ini_options]`, `tox.ini`, `setup.cfg`) and understands the familiar flags (`-v`, `-x`, `-k`, `-m`, `--lf`, `--tb=...`, `-p no:NAME`, ...). It installs alongside pytest without conflict — the `pytest` command is untouched.

### Requirements

- Linux or macOS (no Windows support yet)
- CPython 3.13+ built with a shared libpython — true for uv-managed Pythons, python.org installers, Homebrew, conda, and distro packages. Plain pyenv builds need `PYTHON_CONFIGURE_OPTS="--enable-shared" pyenv install ...`.

### Bundled plugins

The compatibility layers for `pytest-asyncio`, `anyio`'s pytest plugin, `pytest-mock`, `pytest-cov`, `pytest-split`, `pytest-benchmark` and `pytest-xdist`-style `-n` parallelism are built in — no separate plugin installs (the anyio layer runs tests through the installed `anyio` library's backends, so `anyio` itself must be in the environment as usual). Two ways to turn features off:

Per project or per run, like pytest (works with the prebuilt wheel):

```toml
[tool.pytest.ini_options]
addopts = "-p no:benchmark -p no:split"
```

At build time, when installing from source — bundled plugins are Cargo features, all enabled by default:

```toml
[tool.uv]
config-settings-package = { pytest-rs = { build-args = "--no-default-features --features asyncio,mock" } }
```

### Third-party plugins (not reimplemented, loaded as-is)

Installed `pytest11` entry points load through the `pytest` API shim — plugins pytest-rs does **not** reimplement can still work as-is. The supported surface includes fixtures, markers, `pytest_addoption` (plugin `--flags` and ini options), `config.stash`, custom hookspecs (`pytest_addhooks`), `pytest_runtest_protocol`/`pytest_runtest_call` hookwrappers, custom collectors (`pytest_collect_file` → `File`/`Item`), and terminal-reporter replacement (a plugin that unregisters the `terminalreporter` plugin and registers its own subclass takes over the output — pytest-rs suppresses its native rendering and drives the replacement through the same hooks pluggy would). Verified status:

| evidence | plugins |
|---|---|
| own upstream test suite runs under pytest-rs and gates CI (per-suite pass-rate in the **Third-party plugins** [conformance table](#conformance-testing) below) | `pytest-timeout`, `pytest-randomly`, `pytest-env`, `pytest-socket`, `pytest-snapshot`, `pytest-ruff`, `pytest-rerunfailures`, `pytest-order`, `pytest-repeat`, `pytest-instafail`, `pytest-icdiff`, `pytest-metadata`, `pytest-subtests`, `pytest-mypy`, `pytest-bdd`, `pytest-django`; `anyio`'s own plugin module also loads this way |
| functional smoke demo gates CI (`conformance/plugin_smoke.py`) | `Faker`, `time-machine`, `requests-mock`, `inline-snapshot` (snapshot assertions + `--inline-snapshot` flag), `pytest-run-parallel` (`--parallel-threads` really runs each test on N threads) |
| reporter replacement — terminal output byte-diffed against real pytest 9.0.3 | `pytest-pretty`, `pytest-sugar` (progress bar, instant failures; activates on a tty or `--force-sugar`) |
| not reimplemented yet | `pytest-html` (needs the report data model exposed); `syrupy` (serializer/extension framework) |

A plugin that fails to import (e.g. it reaches into pytest/pluggy internals the shim doesn't provide) warns and is skipped without breaking the run. `-p no:NAME` and `PYTEST_DISABLE_PLUGIN_AUTOLOAD` opt out, like pytest.

## Performance

Native startup, collection, fixture orchestration, parallel workers (fork-based) and coverage measurement. Where it pays off:

- suites with heavy fixture/parametrize orchestration and large collections
- `--cov` runs (a native collector instead of a tracing hook)
- `-n` parallel runs (fork workers instead of spawned interpreters)

For small, CPU-bound suites the test bodies dominate and pytest-rs runs at parity with pytest. Try it on your own suite:

```sh
hyperfine -w 1 'pytest -q' 'pytest-rs'
```

## Known limitations

- unix only (no Windows)
- no `--pdb` / debugger integration yet
- third-party pytest plugins are loaded via the `pytest11` entry point and the `pytest` API shim; plugins reaching deep into pytest internals may not work (see "Third-party plugins" above for verified examples)

## Conformance testing

Compatibility is verified by running the **upstream test suites** of the libraries pytest-rs reproduces, unchanged, under pytest-rs (`conformance/`).

Current results (`total = passed + failed + errors + skipped`; updated automatically by `conformance/runner.py`, refreshed by CI on every push to main — see [conformance/RESULTS.md](https://github.com/Yasu-umi/pytest-rs/blob/main/conformance/RESULTS.md) for per-file detail):

<!-- conformance-results:start -->
_linux (CI-verified)_

**pytest & plugin ecosystem** (the APIs pytest-rs reimplements):

| suite | tag | passed | failed | errors | skipped | total | pass % | files all-pass | files run | files excluded |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| pytest | 9.0.3 | 1618 | 526 | 25 | 24 | 2193 | 73.8% | 6 | 45 | 63 |
| pytest-asyncio | v1.4.0 | 268 | 0 | 0 | 0 | 268 | 100.0% | 30 | 30 | 0 |
| pytest-mock | v3.15.1 | 87 | 0 | 0 | 1 | 88 | 98.9% | 1 | 1 | 0 |
| pytest-cov | v7.1.0 | 182 | 24 | 0 | 3 | 209 | 87.1% | 0 | 1 | 0 |
| pytest-xdist | v3.8.0 | 90 | 7 | 0 | 0 | 97 | 92.8% | 0 | 1 | 6 |
| pytest-split | 0.9.0 | 59 | 0 | 0 | 0 | 59 | 100.0% | 1 | 1 | 3 |
| pytest-benchmark | v5.1.0 | 91 | 31 | 0 | 1 | 123 | 74.0% | 4 | 7 | 6 |
| anyio | 4.13.0 | 3120 | 0 | 0 | 42 | 3162 | 98.7% | 26 | 26 | 0 |

**Third-party plugins** (not reimplemented — their own upstream test suites run under pytest-rs, loaded via the `pytest11` entry-point shim):

| suite | tag | passed | failed | errors | skipped | total | pass % | files all-pass | files run | files excluded |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| pytest-timeout | 2.4.0 | 40 | 0 | 0 | 1 | 41 | 97.6% | 1 | 1 | 0 |
| pytest-mypy | v1.0.1 | 0 | 0 | 0 | 0 | 0 | - | 0 | 1 | 0 |
| pytest-ruff | v0.5 | 10 | 0 | 0 | 0 | 10 | 100.0% | 1 | 1 | 0 |
| pytest-subtests | v0.14.2 | 8 | 24 | 0 | 0 | 32 | 25.0% | 0 | 1 | 0 |
| pytest-metadata | v2.0.4 | 6 | 4 | 0 | 0 | 10 | 60.0% | 0 | 1 | 0 |
| pytest-snapshot | v0.9.0 | 100 | 7 | 0 | 0 | 107 | 93.5% | 0 | 3 | 0 |
| pytest-icdiff | 0.5 | 7 | 5 | 0 | 0 | 12 | 58.3% | 0 | 1 | 0 |
| pytest-socket | 0.7.0 | 59 | 6 | 0 | 0 | 65 | 90.8% | 2 | 6 | 0 |
| pytest-order | v1.4.0 | 80 | 54 | 0 | 0 | 134 | 59.7% | 2 | 16 | 0 |
| pytest-repeat | v0.9.4 | 11 | 5 | 0 | 0 | 16 | 68.8% | 0 | 1 | 0 |
| pytest-instafail | v0.5.0 | 45 | 18 | 0 | 0 | 63 | 71.4% | 0 | 1 | 0 |
| pytest-env | 1.6.0 | 66 | 9 | 0 | 0 | 75 | 88.0% | 2 | 3 | 0 |
| pytest-rerunfailures | 9.1.1 | 35 | 12 | 0 | 1 | 48 | 72.9% | 0 | 1 | 0 |
| pytest-randomly | 4.1.0 | 31 | 6 | 0 | 0 | 37 | 83.8% | 0 | 1 | 0 |
| pytest-bdd | 8.1.0 | 36 | 102 | 0 | 1 | 139 | 25.9% | 5 | 35 | 0 |
| pytest-django | v4.9.0 | 146 | 69 | 0 | 1 | 216 | 67.6% | 2 | 13 | 0 |

**Real-world projects** (their suites run unchanged, as drop-in evidence):

| suite | tag | passed | failed | errors | skipped | total | pass % | files all-pass | files run | files excluded |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| click | 8.3.1 | 1314 | 0 | 0 | 21 | 1335 | 98.4% | 20 | 20 | 0 |
| jinja | 3.1.6 | 909 | 0 | 0 | 0 | 909 | 100.0% | 22 | 22 | 0 |
| marshmallow | 4.1.1 | 1119 | 0 | 0 | 0 | 1119 | 100.0% | 12 | 12 | 3 |
| rich | v14.2.0 | 855 | 0 | 0 | 25 | 880 | 97.2% | 60 | 62 | 0 |
<!-- conformance-results:end -->

The suites are included as **shallow git submodules** under `conformance/suites/` at the pinned release tags. Initialize them once after cloning:

```sh
git submodule update --init --depth 1
```

Then run the full conformance harness:

```sh
cargo build
uv run --no-project python conformance/runner.py --local   # uses submodules
uv run --no-project python conformance/runner.py           # re-clones from upstream (CI mode)
```

| Project | License | Tag |
|---|---|---|
| [pytest](https://github.com/pytest-dev/pytest) | MIT | 9.0.3 |
| [pytest-asyncio](https://github.com/pytest-dev/pytest-asyncio) | Apache-2.0 | v1.4.0 |
| [pytest-mock](https://github.com/pytest-dev/pytest-mock) | MIT | v3.15.1 |
| [pytest-cov](https://github.com/pytest-dev/pytest-cov) | MIT | v7.1.0 |
| [pytest-xdist](https://github.com/pytest-dev/pytest-xdist) | MIT | v3.8.0 |
| [pytest-split](https://github.com/jerry-git/pytest-split) | MIT | 0.9.0 |
| [pytest-benchmark](https://github.com/ionelmc/pytest-benchmark) | BSD-2-Clause | v5.1.0 |

pytest-rs reimplements the public APIs of these projects, plus [anyio](https://github.com/agronholm/anyio)'s pytest plugin (MIT). Parts of the bundled Python shims are ports of upstream code; see [THIRD-PARTY-NOTICES.md](https://github.com/Yasu-umi/pytest-rs/blob/main/THIRD-PARTY-NOTICES.md). Credit for the API design and the test suites belongs to their respective authors.

## License

This project is licensed under the MIT License. See the LICENSE file for more details.
