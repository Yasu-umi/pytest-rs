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

Bundled plugins (`asyncio`, `mock`, `cov`, `split`, `benchmark`) are Cargo
features, all enabled by default. To build a binary with only some of them,
pass build args to maturin from the consuming project:

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

Compatibility is verified by running the **upstream test suites** of the libraries pytest-rs reproduces, unchanged, under pytest-rs (`conformance/`). The harness clones each project at a pinned tag at test time; their code is not redistributed in this repository.

| Project | License | Used as |
|---|---|---|
| [pytest](https://github.com/pytest-dev/pytest) | MIT | API reference & conformance suite |
| [pytest-asyncio](https://github.com/pytest-dev/pytest-asyncio) | Apache-2.0 | API reference & conformance suite |
| [pytest-mock](https://github.com/pytest-dev/pytest-mock) | MIT | API reference & conformance suite |
| [pytest-cov](https://github.com/pytest-dev/pytest-cov) | MIT | API reference & conformance suite |
| [pytest-xdist](https://github.com/pytest-dev/pytest-xdist) | MIT | API reference & conformance suite |
| [pytest-split](https://github.com/jerry-git/pytest-split) | MIT | API reference & conformance suite |
| [pytest-benchmark](https://github.com/ionelmc/pytest-benchmark) | BSD-2-Clause | API reference & conformance suite |

pytest-rs reimplements the public APIs of these projects; it does not copy their source code. Credit for the API design and the test suites belongs to their respective authors.

## License

This project is licensed under the MIT License. See the LICENSE file for more details.
