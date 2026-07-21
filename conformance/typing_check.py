"""Typing-parity gate: does the shipped `pytest`/`pytest_asyncio`/`pytest_mock`
API resolve to the SAME precise types real users get from upstream's own
typed stubs (raises() narrowing exc_info, fixture() decorator return type,
mark.parametrize, mocker.patch overloads, etc.)?

This is a separate dimension from conformance/runner.py's runtime pass/fail
suites: it never runs a test, it only asks mypy what type each expression in
conformance/typing/*.py resolves to and diffs that against a `# revealed:
<expected type>` comment on the same line as each `reveal_type(...)` call --
the same methodology typeshed itself uses to pin down its own stub
precision. A silent regression here (e.g. a `**kwargs` collapsing an
overload back to `Any`) would otherwise go unnoticed by every other check in
this repo, since runtime behavior is unaffected.

The corpus is checked directly against crates/*/py (the source tree IS the
shipped package for this project's packaging model -- see pyproject.toml's
[tool.maturin] python-source -- so there is no separate "build" step to
diverge from).

Usage:
    python conformance/typing_check.py
"""

from __future__ import annotations

import os
import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
TYPING_DIR = ROOT / "conformance" / "typing"

MYPYPATH = ":".join(
    str(ROOT / rel)
    for rel in (
        "crates/pytest-rs-core/py",
        "crates/pytest-rs-asyncio/py",
        "crates/pytest-rs-mock/py",
    )
)

REVEAL_CALL_RE = re.compile(r"reveal_type\(.*\)\s*#\s*revealed:\s*(.+?)\s*$")
MYPY_NOTE_RE = re.compile(r"^(?P<file>.+):(?P<line>\d+): note: Revealed type is \"(?P<type>.+)\"$")


def _expected_reveals() -> dict[tuple[str, int], str]:
    """Parse every `reveal_type(...)  # revealed: <type>` line in the corpus."""
    expected: dict[tuple[str, int], str] = {}
    for path in sorted(TYPING_DIR.glob("*.py")):
        rel = str(path.relative_to(ROOT))
        for lineno, line in enumerate(path.read_text().splitlines(), start=1):
            m = REVEAL_CALL_RE.search(line)
            if m:
                expected[(rel, lineno)] = m.group(1)
    return expected


def _run_mypy() -> str:
    env = {**os.environ, "MYPYPATH": MYPYPATH}
    proc = subprocess.run(
        ["uv", "run", "--no-sync", "mypy", "--no-error-summary", str(TYPING_DIR)],
        cwd=ROOT,
        env=env,
        capture_output=True,
        text=True,
    )
    return proc.stdout


def main() -> int:
    expected = _expected_reveals()
    if not expected:
        print(
            f"no `reveal_type(...)  # revealed: ...` lines found under {TYPING_DIR}",
            file=sys.stderr,
        )
        return 1

    stdout = _run_mypy()

    actual: dict[tuple[str, int], str] = {}
    errors: list[str] = []
    for line in stdout.splitlines():
        m = MYPY_NOTE_RE.match(line)
        if m:
            actual[(m.group("file"), int(m.group("line")))] = m.group("type")
        elif ": error:" in line:
            errors.append(line)

    failures = []
    for key, expected_type in expected.items():
        actual_type = actual.get(key)
        if actual_type is None:
            failures.append(
                f"{key[0]}:{key[1]}: expected {expected_type!r}, mypy emitted no reveal_type note"
            )
        elif actual_type != expected_type:
            failures.append(f"{key[0]}:{key[1]}: expected {expected_type!r}, got {actual_type!r}")

    if errors:
        print(
            "unexpected mypy errors in the typing corpus (it should type-check cleanly):",
            file=sys.stderr,
        )
        for e in errors:
            print(f"  {e}", file=sys.stderr)

    if failures:
        print(f"typing parity regressions ({len(failures)}):", file=sys.stderr)
        for f in failures:
            print(f"  {f}", file=sys.stderr)

    if errors or failures:
        return 1

    print(f"typing_check: OK ({len(expected)} reveal_type assertions matched)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
