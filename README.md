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

`pytest-rs` reads the same configuration pytest does (`pytest.ini`, `pyproject.toml` `[tool.pytest.ini_options]`, `tox.ini`, `setup.cfg`) and understands the familiar flags (`-v`, `-x`, `-k`, `-m`, `--lf`, `--tb=...`, `-p no:NAME`, ...). It installs alongside pytest without conflict — the `pytest` command is untouched.

### Requirements

- Linux or macOS (no Windows support yet)
- CPython 3.13+ built with a shared libpython — true for uv-managed Pythons, python.org installers, Homebrew, conda, and distro packages. Plain pyenv builds need `PYTHON_CONFIGURE_OPTS="--enable-shared" pyenv install ...`.

### Bundled plugins

The compatibility layers for `pytest-asyncio`, `pytest-mock`, `pytest-cov`, `pytest-split`, `pytest-benchmark` and `pytest-xdist`-style `-n` parallelism are built in — no separate plugin installs. Two ways to turn features off:

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
- third-party pytest plugins are loaded via the `pytest11` entry point and the `pytest` API shim; plugins reaching deep into pytest internals may not work

## Conformance testing

Compatibility is verified by running the **upstream test suites** of the libraries pytest-rs reproduces, unchanged, under pytest-rs (`conformance/`).

Current results (`total = passed + failed + errors + skipped`; updated automatically by `conformance/runner.py`, refreshed by CI on every push to main — see [conformance/RESULTS.md](https://github.com/Yasu-umi/pytest-rs/blob/main/conformance/RESULTS.md) for per-file detail):

<!-- conformance-results:start -->
_linux (CI-verified)_

**pytest & plugin ecosystem** (the APIs pytest-rs reimplements):

| suite | tag | passed | failed | errors | skipped | total | pass % | files all-pass | files run | files excluded |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| pytest | 9.0.3 | 1186 | 831 | 0 | 22 | 2039 | 58.2% | 5 | 45 | 63 |
| pytest-asyncio | v1.4.0 | 262 | 0 | 1 | 0 | 263 | 99.6% | 29 | 30 | 0 |
| pytest-mock | v3.15.1 | 85 | 0 | 0 | 5 | 90 | 94.4% | 1 | 1 | 0 |
| pytest-cov | v7.1.0 | 105 | 101 | 0 | 3 | 209 | 50.2% | 0 | 1 | 0 |
| pytest-xdist | v3.8.0 | 62 | 35 | 0 | 0 | 97 | 63.9% | 0 | 1 | 6 |
| pytest-split | 0.9.0 | 59 | 0 | 0 | 0 | 59 | 100.0% | 1 | 1 | 3 |
| pytest-benchmark | v5.1.0 | 40 | 82 | 0 | 1 | 123 | 32.5% | 2 | 7 | 6 |

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

pytest-rs reimplements the public APIs of these projects. Parts of the bundled Python shims are ports of upstream code; see [THIRD-PARTY-NOTICES.md](https://github.com/Yasu-umi/pytest-rs/blob/main/THIRD-PARTY-NOTICES.md). Credit for the API design and the test suites belongs to their respective authors.

## License

This project is licensed under the MIT License. See the LICENSE file for more details.
