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

The `pytest-api` target covers what a corpus can miss: every public name
`pytest` exports (and each public attribute of the exported classes) goes
through reveal_type under both MYPYPATHs, and a name whose type holds more
`Any` under the shim is a gap. Without --strict, calling an unannotated
method is no error, so the corpus runs above never notice one; a caller's
`mypy --strict` suite does (`no-untyped-call`, `untyped-decorator`).

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
import ast
import os
import re
import subprocess
import sys
import tempfile
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
    "pytest-api",
    "pytest",
    "pytest-asyncio",
    "pytest-mock",
    "pytest-cov",
    "pytest-xdist",
    "pytest-benchmark",
]

# Not a suite: the public names `pytest` exports, probed with reveal_type
# (see measure_api). An upstream test suite only calls what it happens to
# test, and without --strict an unannotated callee is no error there, so the
# corpus runs above can't see a shim method that lost its types.
API_TARGET = "pytest-api"

# Marks a caller writes as `pytest.mark.<name>`: MarkGenerator declares the
# builtin ones for type checkers only, and the rest resolve through
# __getattr__ (asyncio / anyio are plugin marks, `foo` stands for any custom
# one).
API_MARKS = ["parametrize", "skip", "skipif", "xfail", "usefixtures", "filterwarnings"]
API_MARKS += ["asyncio", "anyio", "foo"]

REVEAL_RE = re.compile(r'^[^:]+:(?P<line>\d+): note: Revealed type is "(?P<type>.*)"$')

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


def _api_probe(real_src: Path) -> list[str]:
    """The probed expressions: every name in pytest's `__all__` and, for a
    class, every public attribute its body defines -- read from the real
    source with ast, so the probe needs none of pytest's runtime deps."""
    init = ast.parse((real_src / "pytest" / "__init__.py").read_text())
    origins: dict[str, tuple[str, str]] = {}
    exported: list[str] = []
    for node in init.body:
        if isinstance(node, ast.ImportFrom) and node.module:
            for alias in node.names:
                origins[alias.asname or alias.name] = (node.module, alias.name)
        elif isinstance(node, ast.Assign) and any(
            isinstance(t, ast.Name) and t.id == "__all__" for t in node.targets
        ):
            exported = list(ast.literal_eval(node.value))
    exprs: list[str] = []
    for name in sorted(exported):
        exprs.append(f"pytest.{name}")
        origin = origins.get(name)
        if origin is None:
            continue
        cls = _find_class(real_src, *origin)
        if cls is not None:
            exprs.extend(f"pytest.{name}.{attr}" for attr in sorted(_public_attrs(cls)))
    exprs.extend(f"pytest.mark.{mark}" for mark in API_MARKS)
    return exprs


def _find_class(real_src: Path, module: str, name: str) -> ast.ClassDef | None:
    base = real_src.joinpath(*module.split("."))
    path = base / "__init__.py" if base.is_dir() else base.with_suffix(".py")
    if not path.is_file():
        return None
    for node in ast.parse(path.read_text()).body:
        if isinstance(node, ast.ClassDef) and node.name == name:
            return node
    return None


def _public_attrs(cls: ast.ClassDef) -> set[str]:
    names: set[str] = set()
    for node in cls.body:
        if isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef, ast.ClassDef)):
            names.add(node.name)
        elif isinstance(node, ast.AnnAssign) and isinstance(node.target, ast.Name):
            names.add(node.target.id)
        elif isinstance(node, ast.Assign):
            names.update(t.id for t in node.targets if isinstance(t, ast.Name))
    return {n for n in names if not n.startswith("_")}


def _reveal(probe: Path, mypypath: Path) -> dict[int, str]:
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
            str(probe),
        ],
        cwd=ROOT,
        env={**os.environ, "MYPYPATH": str(mypypath)},
        capture_output=True,
        text=True,
    )
    revealed: dict[int, str] = {}
    for line in proc.stdout.splitlines():
        match = REVEAL_RE.match(line)
        if match is not None:
            revealed[int(match.group("line"))] = match.group("type")
    return revealed


def measure_api(pytest_suite: Suite) -> tuple[Findings, Findings]:
    """(shim-only, real-only) findings for pytest's public names.

    A name is a gap when its revealed type holds more `Any` under the shim
    than under real pytest: an unannotated parameter or return, a module
    attribute with no declared type, a mark that is an untyped decorator.
    The revealed types themselves are not compared -- the classes live in
    different modules (`_pytest.monkeypatch` vs `pytest._monkeypatch`)."""
    exprs = _api_probe(_real_src(pytest_suite))
    with tempfile.TemporaryDirectory() as tmp:
        probe = Path(tmp) / "pytest_api_probe.py"
        probe.write_text("import pytest\n\n" + "".join(f"reveal_type({e})\n" for e in exprs))
        real = _reveal(probe, _real_src(pytest_suite))
        shim = _reveal(probe, SHIM_ROOT)
    shim_only: Findings = set()
    real_only: Findings = set()
    for lineno, expr in enumerate(exprs, start=3):
        real_any = real.get(lineno, "Any").count("Any")
        shim_any = shim.get(lineno, "Any").count("Any")
        if shim_any > real_any:
            shim_only.add((expr, "reveals more Any than pytest", "api-any"))
        elif real_any > shim_any:
            real_only.add((expr, "reveals less Any than pytest", "api-any"))
    return shim_only, real_only


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
        "# py.path.local -- there is no class here to point it at. Private",
        "# internals upstream's own tests reach into (Config._do_configure,",
        "# Session._setupstate, ...) and attributes the shim lacks at runtime too",
        "# (Traceback.cut, Class.cls, ...) are API gaps, not typing ones. Under",
        "# [pytest-api], the bare class names are constructors whose shim",
        "# signature differs from upstream's.",
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
        if name == API_TARGET:
            shim_only, real_only = measure_api(pytest_suite)
        else:
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
