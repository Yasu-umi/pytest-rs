# Third-party plugin example

A project that uses a pytest plugin pytest-rs does **not** reimplement
(`pytest-aiohttp`), set up so that installing it does not drag the real `pytest`
distribution back in.

## The problem this example solves

Every pytest plugin declares a dependency on `pytest`. pytest-rs ships its own
importable `pytest` package, so a plain `uv add --dev pytest-aiohttp` resolves
that dependency by installing the upstream distribution *over* pytest-rs's files
in `site-packages/pytest/` — with no warning from pip or uv, since neither knows
the two distributions claim the same import path.

## The setup

[`pyproject.toml`](pyproject.toml) keeps the plugin as a normal dev dependency
and overrides only the requirements pytest-rs already provides, using a marker
that matches nowhere:

```toml
[dependency-groups]
dev = ["pytest-rs>=0.0.12", "pytest-aiohttp==1.1.1"]

[tool.uv]
override-dependencies = [
    "pytest ; sys_platform == 'never'",
    "pytest-asyncio ; sys_platform == 'never'",
]
```

`pytest-aiohttp` requires `pytest`, `pytest-asyncio` and `aiohttp`. The first two
are pytest-rs's job — `pytest-rs>=0.0.12` installs both import paths — so they
are overridden away; `aiohttp` is an ordinary package and installs normally.

## Run it

```sh
uv sync
uv run pytest-rs -v
```

Both tests use fixtures that come from the plugin (`aiohttp_client`,
`aiohttp_server`) with async bodies driven by pytest-rs's native asyncio support.

## Check that upstream `pytest` stayed out

```sh
uv pip list | grep -i pytest
```

Expected — the plugin and pytest-rs, and no bare `pytest` line:

```
pytest-aiohttp    1.1.1
pytest-rs         0.0.12
```

A `pytest` line means the upstream distribution got installed and has overwritten
part of pytest-rs's `pytest/` directory; `uv pip install --force-reinstall
pytest-rs` after removing it restores the files.

## Adapting this to another plugin

Override whichever of pytest-rs's own bundled distributions the plugin asks for —
`pytest`, `pluggy`, `iniconfig`, plus any of the
[bundled plugins](../../README.md#bundled-plugins) (`pytest-asyncio` here; a
plugin building on `pytest-xdist` or `pytest-cov` would name those). Leave every
other dependency alone. `uv pip tree --package <plugin>` shows what to look at.

For a one-off install outside a locked project, the imperative equivalent is:

```sh
uv pip install --no-deps pytest-aiohttp==1.1.1   # or: pip install --no-deps ...
uv pip install aiohttp                           # its non-pytest dependencies
```

`--no-deps` skips *all* requirements, so the dependencies that are not
pytest-rs's job have to be named explicitly.
