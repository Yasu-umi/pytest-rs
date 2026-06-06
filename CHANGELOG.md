# Changelog

Notable changes per release. The release workflow uses the matching section
as the GitHub release notes (auto-generated notes are the fallback).

## v0.0.1 (unreleased)

First public release: a drop-in compatible pytest runner written in Rust,
focused on speed.

- Import-based collection, fixtures (function/module/session scopes, autouse,
  generator teardown), parametrize, marks, skip/xfail, assertion rewriting
  with pytest-style failure explanations
- Bundled plugin compatibility: pytest-asyncio, pytest-mock, pytest-cov,
  pytest-split, pytest-benchmark, and `-n` parallel runs (fork-based on unix)
- pytest-compatible terminal output, exit codes, junitxml, log-cli, capture
  (`fd`/`sys`/`tee-sys`)
- Conformance: upstream test suites of pytest and its plugin ecosystem run
  unchanged under pytest-rs, scored and gated in CI — see
  [conformance/RESULTS.md](conformance/RESULTS.md)
- Distributed as prebuilt wheels (linux x86_64/aarch64, macOS arm64;
  CPython 3.13/3.14); the `pytest-rs` command resolves the environment's
  libpython at startup

Known limitations: unix only (no Windows), no `--pdb`, CPython with a shared
libpython required (default pyenv builds need `--enable-shared`).
