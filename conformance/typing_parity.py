"""Differential typing check: do the shipped shims give callers the types the
real libraries do?

For each natively reimplemented plugin, mypy runs twice over the *same* corpus
-- that library's own test suite, already checked out for the conformance run
-- changing only what MYPYPATH resolves the implementation to:

    real:  conformance/suites/<lib>/src  (+ real pytest's src)
    shim:  crates/pytest-rs-core/py      (pytest, _pytest, pytest_asyncio, ...)

Errors the shim run reports and the real run doesn't are typing-parity gaps:
same source, same mypy, so the declared types are the only variable. The
expectation is *generated* rather than hand-written -- the real library is the
expectation -- which is why this catches surface that conformance/typing's
reveal_type corpus cannot: nobody writes an assertion for the attribute they
forgot to declare in the first place (`pytest.FixtureRequest` shipped as an
empty placeholder for several releases that way).

Usage:
    uv run python conformance/typing_parity.py                 # report
    uv run python conformance/typing_parity.py --check         # gate on the baseline
    uv run python conformance/typing_parity.py --update        # refresh the baseline
    uv run python conformance/typing_parity.py --suite pytest  # one target

`--check` fails on a shim-only error that is not in
conformance/expected/typing_parity.toml, and tells you to refresh the baseline
when a recorded one is gone. Errors reported only by the *real* run are
recorded too but never gate: they mean the shim is more permissive, which
costs a caller nothing.
"""

import argparse
import os
import re
import subprocess
import sys
import tomllib
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from runner import ROOT, Suite, load_suites  # noqa: E402

BASELINE = ROOT / "conformance" / "expected" / "typing_parity.toml"

# Matches [tool.mypy]'s python_version: the config file is bypassed (see
# _run_mypy), so the version has to be restated rather than inherited.
PYTHON_VERSION = "3.13"

# The importable shims all live under one root (the same layout the wheel
# installs into site-packages, see pyproject.toml's [tool.maturin]).
SHIM_ROOT = ROOT / "crates" / "pytest-rs-core" / "py"

# Suites whose corpus exercises a shim we ship. pytest-split is absent on
# purpose: it has no importable API at all (--splits/--group are CLI-only), so
# there is no typing surface to compare.
TARGETS = [
    "pytest",
    "pytest-asyncio",
    "pytest-mock",
    "pytest-cov",
    "pytest-xdist",
    "pytest-benchmark",
]

# "file:line:col: error: message  [code]" -- the column is optional.
ERROR_RE = re.compile(
    r"^(?P<file>[^:]+):(?P<line>\d+):(?:\d+:)? error: (?P<msg>.*?)(?:\s+\[(?P<code>[\w-]+)\])?$"
)

Findings = set[tuple[str, str, str]]


def _real_src(suite: Suite) -> Path:
    """The real library's import root inside its checkout."""
    src = suite.checkout / "src"
    return src if src.is_dir() else suite.checkout


def _corpus(suite: Suite) -> list[Path]:
    return [suite.checkout / p for p in suite.testpaths]


def _run_mypy(corpus: list[Path], mypypath: list[Path], checkout: Path) -> Findings:
    """mypy over `corpus`, resolving imports through `mypypath`.

    Two settings matter, and both exist so that a shim we do not ship *at all*
    registers as a gap instead of perfect parity: no --ignore-missing-imports
    (an unresolvable module must error, not silently become `Any`, or a missing
    package reads as fewer errors than the real library), and --config-file
    /dev/null so this repo's own [tool.mypy] -- which sets exactly that flag,
    plus its own `files` -- cannot leak in. --no-site-packages keeps the dev
    venv's installed distributions out too, so MYPYPATH plus the stdlib is the
    whole difference between the two runs. Unresolved third-party test deps
    (execnet, elasticsearch, ...) are then missing from both runs and cancel in
    the diff; only the target library differs.
    """
    proc = subprocess.run(
        [
            "uv",
            "run",
            "--no-sync",
            "mypy",
            "--no-error-summary",
            "--no-incremental",
            "--config-file",
            os.devnull,
            "--no-site-packages",
            "--python-version",
            PYTHON_VERSION,
            *(str(p) for p in corpus),
        ],
        cwd=ROOT,
        env={**os.environ, "MYPYPATH": ":".join(str(p) for p in mypypath)},
        capture_output=True,
        text=True,
    )
    findings: Findings = set()
    for line in proc.stdout.splitlines():
        match = ERROR_RE.match(line)
        if match is None:
            continue
        path = match.group("file")
        # Errors inside an implementation are that implementation's own
        # business; only what a *caller* sees is comparable.
        if not any(part in path for part in ("/testing/", "/tests/", "/test/")):
            continue
        # Line numbers move between tags; the (file, message, code) triple is
        # what identifies a gap.
        findings.add((_relative(path, checkout), match.group("msg"), match.group("code") or ""))
    return findings


def _relative(path: str, checkout: Path) -> str:
    """Checkout-relative path, so the baseline survives a tag bump (the
    checkout directory itself is named `<suite>-<tag>`)."""
    absolute = (ROOT / path).resolve()
    try:
        return str(absolute.relative_to(checkout.resolve()))
    except ValueError:
        return path


def measure(suite: Suite, pytest_suite: Suite) -> tuple[Findings, Findings]:
    """(shim-only, real-only) findings for one suite.

    Every plugin's tests import pytest itself, so the real run needs real
    pytest's source alongside the plugin's -- otherwise its `pytest` would
    resolve to whatever is installed in this repo's dev venv.
    """
    corpus = _corpus(suite)
    real = _run_mypy(corpus, [_real_src(pytest_suite), _real_src(suite)], suite.checkout)
    shim = _run_mypy(corpus, [SHIM_ROOT], suite.checkout)
    return shim - real, real - shim


def _format(findings: Findings) -> list[str]:
    return sorted(f"{path}: [{code}] {msg}" for path, msg, code in findings)


def load_baseline() -> dict[str, dict[str, list[str]]]:
    if not BASELINE.exists():
        return {}
    return tomllib.loads(BASELINE.read_text())


def write_baseline(measured: dict[str, tuple[Findings, Findings]]) -> None:
    lines = [
        "# Generated by conformance/typing_parity.py --update. Each entry is an",
        "# error a caller gets from the shipped shim but not from the real",
        "# library: an accepted typing-parity gap. shim_only shrinking is the",
        "# goal; a new entry is a regression the --check gate rejects.",
        "#",
        "# What stays here on purpose: internals that exist in pytest-rs only as",
        "# Rust (xdist.workermanage, pytest_benchmark.utils, FormattedExcinfo's",
        "# formatting methods, _pytest._code's Source/Frame navigation). Shipping",
        "# Python modules so an upstream test can import them would mean shipping",
        "# code nothing runs; a caller's own suite reaches none of them. Also",
        "# permanent: `import py`, whose compat module upstream bundles and",
        "# pytest-rs does not, and LEGACY_PATH, which upstream binds to",
        "# py.path.local -- there is no class here to point it at.",
        "#",
        "# real_only entries are the opposite (the shim is more permissive than",
        "# the real library); they cost a caller nothing and never gate.",
        "",
    ]
    for name, (shim_only, real_only) in measured.items():
        lines.append(f"[{name}]")
        for key, findings in (("shim_only", shim_only), ("real_only", real_only)):
            if not findings:
                lines.append(f"{key} = []")
                continue
            lines.append(f"{key} = [")
            lines.extend(f"    {_toml_str(entry)}," for entry in _format(findings))
            lines.append("]")
        lines.append("")
    BASELINE.write_text("\n".join(lines))


def _toml_str(value: str) -> str:
    escaped = value.replace("\\", "\\\\").replace('"', '\\"')
    return f'"{escaped}"'


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--check", action="store_true", help="gate against the baseline")
    parser.add_argument("--update", action="store_true", help="rewrite the baseline")
    parser.add_argument("--suite", help="comma-separated subset of the targets")
    parser.add_argument("--list", action="store_true", help="print every shim-only error")
    args = parser.parse_args()

    names = [n.strip() for n in args.suite.split(",")] if args.suite else TARGETS
    unknown = set(names) - set(TARGETS)
    if unknown:
        sys.exit(f"not a typing-parity target: {', '.join(sorted(unknown))}")

    baseline = load_baseline()
    measured: dict[str, tuple[Findings, Findings]] = {}
    failures: list[str] = []
    stale: list[str] = []

    (pytest_suite,) = load_suites("pytest")
    pytest_suite.fetch(use_local=False)

    for name in names:
        (suite,) = load_suites(name)
        suite.fetch(use_local=False)
        shim_only, real_only = measure(suite, pytest_suite)
        measured[name] = (shim_only, real_only)
        recorded = set(baseline.get(name, {}).get("shim_only", []))
        actual = set(_format(shim_only))
        failures.extend(f"{name}: {entry}" for entry in sorted(actual - recorded))
        stale.extend(f"{name}: {entry}" for entry in sorted(recorded - actual))
        print(
            f"{name:18} shim-only {len(shim_only):4d}  real-only {len(real_only):4d}"
            f"  (baseline {len(recorded)})"
        )
        if args.list:
            for entry in _format(shim_only):
                print(f"    {entry}")

    if args.update:
        # Only a full run may rewrite the file; a subset would drop the rest.
        if set(names) != set(TARGETS):
            sys.exit("--update needs the full target set (drop --suite)")
        write_baseline(measured)
        print(f"wrote {BASELINE.relative_to(ROOT)}")
        return 0

    if args.check:
        if failures:
            print(f"\nnew typing-parity gaps ({len(failures)}):", file=sys.stderr)
            for entry in failures:
                print(f"  {entry}", file=sys.stderr)
        if stale:
            print(
                f"\nbaseline entries no longer reproduced ({len(stale)}) -- rerun with"
                " --update to record the fix:",
                file=sys.stderr,
            )
            for entry in stale:
                print(f"  {entry}", file=sys.stderr)
        if failures or stale:
            return 1
        print("typing_parity: OK (no new gaps)")

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
