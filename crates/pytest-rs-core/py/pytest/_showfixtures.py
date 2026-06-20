"""Render `--fixtures` and `--fixtures-per-test` output.

The engine collects fixtures into its Rust registry, then (when one of these
flags is set) hands the registered defs / per-item closures here for display.
The formatting mirrors `_pytest.fixtures._showfixtures_main` and
`_show_fixtures_per_test` so the output matches upstream byte-for-byte.
"""

from __future__ import annotations

import inspect
import os
from pathlib import Path


def _get_real_func(func):
    """Unwrap __wrapped__ chains (pytest's get_real_func), so a fixture's
    reported location is its user `def`, not a decorator wrapper."""
    seen = set()
    while hasattr(func, "__wrapped__") and id(func) not in seen:
        seen.add(id(func))
        func = func.__wrapped__
    return func


def _getlocation(func, curdir: str) -> str:
    func = _get_real_func(func)
    try:
        fn = Path(inspect.getfile(func))
    except TypeError:
        return repr(func)
    lineno = func.__code__.co_firstlineno
    try:
        relfn = fn.relative_to(curdir)
    except ValueError:
        return f"{fn}:{lineno + 1}"
    return f"{relfn}:{lineno + 1}"


def _bestrelpath(directory: Path, dest: Path) -> str:
    if dest == directory:
        return os.curdir
    try:
        base = Path(os.path.commonpath([directory, dest]))
    except ValueError:
        return str(dest)
    reldirectory = directory.relative_to(base)
    reldest = dest.relative_to(base)
    return os.path.join(
        *([os.pardir] * len(reldirectory.parts)),
        *reldest.parts,
    )


# The bundled _pytest dir; builtin fixtures defined there get the ".../_pytest"
# prefix pytest shows instead of an absolute path.
_PYTEST_DIR: Path | None
try:
    import _pytest

    _PYTEST_DIR = Path(_pytest.__file__).parent
except Exception:  # pragma: no cover - _pytest always importable in practice
    _PYTEST_DIR = None


def fixturedef_line(func, rootdir: str) -> str:
    """`{relpath}:{lineno}:  def {name}{signature}` for a fixture factory,
    matching `FixtureRequest._format_fixturedef_line` (the lines a ScopeMismatch
    prints). `lineno` is the factory's `co_firstlineno` (pytest's getfslineno is
    0-based and the formatter adds 1), i.e. the decorator line."""
    real = _get_real_func(func)
    try:
        path = Path(inspect.getfile(real))
        relpath = _bestrelpath(Path(rootdir), path)
    except TypeError:
        relpath = repr(real)
    lineno = getattr(getattr(real, "__code__", None), "co_firstlineno", 0)
    try:
        sig = str(inspect.signature(real))
    except (ValueError, TypeError):
        sig = "(...)"
    return f"{relpath}:{lineno}:  def {getattr(real, '__name__', '<fixture>')}{sig}"


def _pretty_fixture_path(invocation_dir: str, func) -> str:
    loc = Path(_getlocation(func, invocation_dir))
    if _PYTEST_DIR is not None:
        prefix = Path("...", "_pytest")
        try:
            return str(prefix / loc.relative_to(_PYTEST_DIR))
        except ValueError:
            pass
    return _bestrelpath(Path(invocation_dir), loc)


def _sep(title: str, sepchar: str = "-", fullwidth: int = 80) -> str:
    """pytest TerminalWriter.sep: title centered in a sepchar fill."""
    n = max((fullwidth - len(title) - 2) // len(sepchar), 1)
    line = f"{sepchar * n} {title} {sepchar * n}"
    if len(line) + len(sepchar) <= fullwidth:
        line += sepchar
    return line


def _write_docstring(out: list[str], doc: str, indent: str = "    ") -> None:
    for line in doc.split("\n"):
        out.append(indent + line)


def _markup(text: str, code: int, color: bool) -> str:
    """Wrap `text` in an SGR escape + reset when color is on, matching
    TerminalWriter.write(text, <color>=True) (each segment its own escape)."""
    if not color:
        return text
    return f"\x1b[{code}m{text}\x1b[0m"


def _write_fixture(
    out: list[str], argname, scope, func, invocation_dir, verbose, *, show_scope, color=False
):
    prettypath = _pretty_fixture_path(invocation_dir, func)
    # pytest colors the name green, the scope cyan, and the location yellow.
    head = _markup(argname, 32, color)
    if show_scope and scope != "function":
        head += _markup(f" [{scope} scope]", 36, color)
    out.append(head + _markup(f" -- {prettypath}", 33, color))
    doc = inspect.getdoc(func)
    if doc:
        _write_docstring(out, doc.split("\n\n", maxsplit=1)[0] if verbose <= 0 else doc)
    else:
        out.append(_markup("    no docstring available", 31, color))


def show_fixtures(defs, invocation_dir: str, verbose: int, color: bool = False) -> str:
    """`--fixtures`: list every registered fixture, grouped by defining module.

    `defs` is an iterable of (argname, scope, baseid, func) from the registry,
    in registration order.
    """
    available = []
    seen: set[tuple[str, str]] = set()
    for argname, scope, baseid, func in defs:
        loc = _getlocation(func, invocation_dir)
        if (argname, loc) in seen:
            continue
        seen.add((argname, loc))
        module = getattr(_get_real_func(func), "__module__", "") or ""
        # A conftest re-imported under a unique alias (conftest@<hash>) when its
        # plain name was already taken should still display as "conftest" in the
        # "fixtures defined from" header (pytest uses the node baseid, not the
        # mangled import name).
        module = module.split("@", 1)[0]
        # pytest's builtins live in `_pytest.*`; pytest-rs ships them as `pytest._*`
        # (plus internal `_pytest_rs*` plugins). Treat both like pytest does: no
        # "fixtures defined from" separator, and sorted ahead of user modules.
        is_builtin = module.startswith(("_pytest.", "pytest._", "_pytest_rs"))
        available.append(
            (
                len(baseid),
                0 if is_builtin else 1,
                module,
                _pretty_fixture_path(invocation_dir, func),
                argname,
                scope,
                func,
                is_builtin,
            )
        )
    available.sort(key=lambda t: (t[0], t[1], t[2], t[3], t[4]))

    out: list[str] = []
    currentmodule = None
    for _baseid, _rank, module, _prettypath, argname, scope, func, is_builtin in available:
        if currentmodule != module and not is_builtin:
            out.append("")
            out.append(_sep(f"fixtures defined from {module}"))
            currentmodule = module
        if verbose <= 0 and argname.startswith("_"):
            continue
        _write_fixture(
            out, argname, scope, func, invocation_dir, verbose, show_scope=True, color=color
        )
        out.append("")
    return "\n".join(out)


def show_fixtures_per_test(items, invocation_dir: str, verbose: int, color: bool = False) -> str:
    """`--fixtures-per-test`: per test item, list the fixtures it uses.

    `items` is an iterable of (item_name, item_func, closure) where closure is
    a list of (argname, scope, baseid, func) for the item's fixture closure.
    """
    out: list[str] = []
    for item_name, item_func, closure in items:
        if not closure:
            continue
        out.append("")
        out.append(_sep(f"fixtures used by {item_name}"))
        # pytest's get_best_relpath keeps the trailing :lineno (it feeds the
        # whole "file:lineno" string through bestrelpath, which leaves a
        # relative path untouched).
        loc = _getlocation(item_func, invocation_dir) if item_func is not None else ""
        out.append(_sep(f"({loc})"))
        for argname, scope, _baseid, func in sorted(closure, key=lambda t: t[0]):
            if verbose <= 0 and argname.startswith("_"):
                continue
            _write_fixture(
                out, argname, scope, func, invocation_dir, verbose, show_scope=False, color=color
            )
    return "\n".join(out)
