# pytest-rs

pytest-rs is a re-implementation of the popular Python testing framework [pytest](https://github.com/pytest-dev/pytest) in Rust, focused on **speed**: a drop-in compatible runner where startup, collection, fixture orchestration, coverage measurement, and reporting are native code, while test bodies run on embedded CPython.

> Note: This project is currently in active development (alpha stage). Many features are still under implementation and subject to change. See [docs/DESIGN.md](docs/DESIGN.md) for the architecture and roadmap.

## Status

- Import-based collection (`test_*.py`, `Test*` classes, `conftest.py`)
- Fixtures: function/module/session scopes, autouse, generator teardown, dependencies
- `@pytest.mark.parametrize`, `@pytest.mark.skip`, `pytest.raises` / `approx` / `skip` / `fail`
- async tests & fixtures via the bundled `pytest-rs-asyncio` plugin (strict/auto mode)
- pytest-compatible terminal output and exit codes
- Plugin system: Rust traits mirroring pytest hooks, plugins as crates behind feature flags

## Installation

pytest-rs builds as a single binary via [maturin](https://github.com/PyO3/maturin); install it like any Python package:

```toml
[dependency-groups]
dev = ["pytest-rs"]

[tool.uv.sources]
pytest-rs = { path = "../pytest-rs" }  # or a published index
```

### Selecting plugins at install time

Bundled plugins (`asyncio`, `mock`, `cov`, `split`, `benchmark`, `xdist`)
are Cargo features, all enabled by default. To build a binary with only some
of them, pass build args to maturin from the consuming project:

```toml
[tool.uv]
config-settings-package = { pytest-rs = { build-args = "--no-default-features --features asyncio,mock" } }
```

### Disabling plugins at runtime

Like pytest, any bundled plugin can be turned off per run or per project
without rebuilding:

```toml
[tool.pytest.ini_options]
addopts = "-p no:benchmark -p no:split"
```

## Conformance testing

Compatibility is verified by running the **upstream test suites** of the libraries pytest-rs reproduces, unchanged, under pytest-rs (`conformance/`).

Current results (`total = passed + failed + errors + skipped`; updated automatically by `conformance/runner.py`, refreshed by CI on every push to main — see [conformance/RESULTS.md](conformance/RESULTS.md) for per-file detail):

<!-- conformance-results:start -->
_linux (CI-verified)_

| suite | tag | passed | failed | errors | skipped | total | pass % | files all-pass | files run | files excluded |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| pytest | 9.0.3 | 1182 | 835 | 0 | 22 | 2039 | 58.0% | 5 | 45 | 63 |
| pytest-asyncio | v1.4.0 | 262 | 0 | 1 | 0 | 263 | 99.6% | 29 | 30 | 0 |
| pytest-mock | v3.15.1 | 85 | 0 | 0 | 5 | 90 | 94.4% | 1 | 1 | 0 |
| pytest-cov | v7.1.0 | 47 | 159 | 0 | 3 | 209 | 22.5% | 0 | 1 | 0 |
| pytest-xdist | v3.8.0 | 62 | 36 | 0 | 0 | 98 | 63.3% | 0 | 1 | 6 |
| pytest-split | 0.9.0 | 59 | 0 | 0 | 0 | 59 | 100.0% | 1 | 1 | 3 |
| pytest-benchmark | v5.1.0 | 40 | 82 | 0 | 1 | 123 | 32.5% | 2 | 7 | 6 |
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

pytest-rs reimplements the public APIs of these projects; it does not copy their source code. Credit for the API design and the test suites belongs to their respective authors.

## License

This project is licensed under the MIT License. See the LICENSE file for more details.
