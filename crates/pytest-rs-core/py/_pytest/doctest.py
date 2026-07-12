"""Doctest collection and execution for pytest-rs.

Called from Rust after module collection when --doctest-modules is active,
and for text files matching --doctest-glob patterns.
"""

from __future__ import annotations

import contextlib
import doctest
import fnmatch
import importlib.util
import inspect
import os
import re
import sys
import traceback as tb_mod
import types
import warnings
from functools import cached_property
from typing import Any

from _pytest.outcomes import skip

# ---------------------------------------------------------------------------
# Exceptions
# ---------------------------------------------------------------------------


def _format_context_lines(dtest: doctest.DocTest, ex: doctest.Example) -> list[str]:
    """Return source-context lines for a failing doctest example.

    Shows up to 10 lines from the docstring ending at the failing example,
    with ``>>> ``/``... `` prompts added and 1-indexed absolute line numbers.
    When lineno information is unavailable, shows a placeholder message.
    """
    if dtest.lineno is None:
        lines: list[str] = ["EXAMPLE LOCATION UNKNOWN, not showing all tests of that example"]
        src_lines = ex.source.splitlines() or [""]
        for i, src_line in enumerate(src_lines):
            prompt = ">>> " if i == 0 else "... "
            lines.append(f"??? {prompt}{src_line}")
        return lines

    docstring_lines = (dtest.docstring or "").splitlines()

    # Map offset→(prompt, code) for every source line of every example.
    source_at: dict = {}
    for e in dtest.examples:
        for i, src_line in enumerate(e.source.splitlines()):
            source_at[e.lineno + i] = (">>> " if i == 0 else "... ", src_line)

    # Window: show at most 10 lines ending at the last source line of ex.
    ex_src_lines = ex.source.splitlines() or [""]
    window_end = ex.lineno + len(ex_src_lines) - 1
    window_start = max(0, ex.lineno - 9)

    result: list[str] = []
    for offset in range(window_start, window_end + 1):
        abs_line = dtest.lineno + offset + 1
        if offset in source_at:
            prompt, code = source_at[offset]
            result.append(f"{abs_line:03d} {prompt}{code}")
        else:
            content = docstring_lines[offset] if offset < len(docstring_lines) else ""
            result.append(f"{abs_line:03d} {content}")
    return result


def _location_line(dtest: doctest.DocTest, ex: doctest.Example, kind: str) -> str:
    if dtest.filename:
        if dtest.lineno is not None:
            lineno: Any = dtest.lineno + ex.lineno + 1
        else:
            lineno = None
        return f"{dtest.filename}:{lineno}: {kind}"
    return ""


class DoctestFailure(Exception):
    """Raised when a doctest example fails; formatted without a Python traceback."""

    pytrace = False

    def __init__(
        self,
        dtest: doctest.DocTest,
        failure: doctest.DocTestFailure,
        optionflags: int = 0,
        checker: Any = None,
    ) -> None:
        self.dtest = dtest
        self.failure = failure
        self.optionflags = optionflags
        self._checker = checker or doctest.OutputChecker()
        super().__init__(str(failure))

    @property
    def msg(self) -> str:
        return self._format()

    def _format(self) -> str:
        ex = self.failure.example
        got = self.failure.got
        lines = _format_context_lines(self.dtest, ex)
        diff = self._checker.output_difference(ex, got or "", self.optionflags)
        lines.append(diff.rstrip())
        loc = _location_line(self.dtest, ex, "DocTestFailure")
        if loc:
            lines.append("")
            lines.append(loc)
        return "\n".join(lines)


class DoctestUnexpected(Exception):
    """Raised when a doctest example triggers an unexpected exception."""

    pytrace = False

    def __init__(
        self,
        test: doctest.DocTest,
        example: doctest.Example,
        exc_info: tuple,
        optionflags: int = 0,
    ) -> None:
        self._test = test
        self._example = example
        self._exc_info = exc_info
        self.optionflags = optionflags
        super().__init__(str(exc_info[1]))

    @property
    def msg(self) -> str:
        return self._format()

    def _format(self) -> str:
        exc_type, exc_val, exc_tb = self._exc_info
        lines = _format_context_lines(self._test, self._example)
        lines.append(f"UNEXPECTED EXCEPTION: {exc_type.__name__}({exc_val})")
        tb_text = "".join(tb_mod.format_exception(exc_type, exc_val, exc_tb)).rstrip()
        lines.extend(tb_text.splitlines())
        loc = _location_line(self._test, self._example, "UnexpectedException")
        if loc:
            lines.append(loc)
        return "\n".join(lines)


def _format_unexpected(failure: doctest.UnexpectedException) -> str:
    """Format a doctest.UnexpectedException (used in continue-on-failure mode)."""
    test = failure.test
    ex = failure.example
    exc_type, exc_val, exc_tb = failure.exc_info
    lines = _format_context_lines(test, ex)
    lines.append(f"UNEXPECTED EXCEPTION: {exc_type.__name__}({exc_val})")
    tb_text = "".join(tb_mod.format_exception(exc_type, exc_val, exc_tb)).rstrip()
    lines.extend(tb_text.splitlines())
    loc = _location_line(test, ex, "UnexpectedException")
    if loc:
        lines.append(loc)
    return "\n".join(lines)


def _format_one(f: Any) -> str:
    if isinstance(f, DoctestFailure):
        return f._format()
    if isinstance(f, DoctestUnexpected):
        return f._format()
    if isinstance(f, doctest.UnexpectedException):
        return _format_unexpected(f)
    return str(f)


class MultipleDoctestFailures(Exception):
    """Raised when continue_on_failure=True and multiple examples failed."""

    pytrace = False

    def __init__(self, failures: list) -> None:
        self.failures = failures
        super().__init__(f"{len(failures)} doctest failure(s)")

    @property
    def msg(self) -> str:
        return "\n\n".join(_format_one(f) for f in self.failures)


# ---------------------------------------------------------------------------
# Option flags
# ---------------------------------------------------------------------------

# Register pytest's extended flags (ALLOW_UNICODE / ALLOW_BYTES / NUMBER) into
# the doctest module so inline +ALLOW_BYTES etc. comments are recognized.
_ALLOW_UNICODE = 0
_ALLOW_BYTES = 0
_NUMBER = 0

for _name, _bits in [("ALLOW_UNICODE", 1 << 16), ("ALLOW_BYTES", 1 << 17), ("NUMBER", 1 << 18)]:
    if not hasattr(doctest, _name):
        setattr(doctest, _name, _bits)
        doctest.OPTIONFLAGS_BY_NAME[_name] = _bits
    exec(f"_{_name} = doctest.{_name}")

_OPTION_FLAGS_NAMES = {
    "DONT_ACCEPT_TRUE_FOR_1": doctest.DONT_ACCEPT_TRUE_FOR_1,
    "DONT_ACCEPT_BLANKLINE": doctest.DONT_ACCEPT_BLANKLINE,
    "NORMALIZE_WHITESPACE": doctest.NORMALIZE_WHITESPACE,
    "ELLIPSIS": doctest.ELLIPSIS,
    "SKIP": doctest.SKIP,
    "IGNORE_EXCEPTION_DETAIL": doctest.IGNORE_EXCEPTION_DETAIL,
    "REPORT_UDIFF": doctest.REPORT_UDIFF,
    "REPORT_CDIFF": doctest.REPORT_CDIFF,
    "REPORT_NDIFF": doctest.REPORT_NDIFF,
    "REPORT_ONLY_FIRST_FAILURE": doctest.REPORT_ONLY_FIRST_FAILURE,
    "ALLOW_UNICODE": _ALLOW_UNICODE,
    "ALLOW_BYTES": _ALLOW_BYTES,
    "NUMBER": _NUMBER,
}

_REPORT_FLAG_MAP = {
    "udiff": doctest.REPORT_UDIFF,
    "cdiff": doctest.REPORT_CDIFF,
    "ndiff": doctest.REPORT_NDIFF,
    "only_first_failure": doctest.REPORT_ONLY_FIRST_FAILURE,
    "none": 0,
}


def get_optionflags(config: Any) -> int:
    flags = 0
    raw = config.getini("doctest_optionflags") if hasattr(config, "getini") else []
    # getini returns a string (space/newline-separated) or a list depending on the caller.
    if isinstance(raw, str):
        ini_flags = raw.split() if raw else []
    else:
        ini_flags = list(raw) if raw else []
    if not ini_flags:
        ini_flags = ["ELLIPSIS"]
    for name in ini_flags:
        flag = _OPTION_FLAGS_NAMES.get(name)
        if flag is not None:
            flags |= flag
    report_choice = _get_report_choice(config)
    flags |= report_choice
    return flags


def _get_report_choice(config: Any) -> int:
    key = None
    try:
        key = config.getoption("doctest_report")
    except Exception:
        pass
    if key is None:
        key = "none"
    return _REPORT_FLAG_MAP.get(str(key).lower(), 0)


def _get_continue_on_failure(config: Any) -> bool:
    try:
        return bool(config.getoption("doctest_continue_on_failure"))
    except Exception:
        return False


# ---------------------------------------------------------------------------
# Output checker with ALLOW_UNICODE / ALLOW_BYTES / NUMBER
# ---------------------------------------------------------------------------

_NUMBER_RE = re.compile(
    r"""
    (?P<number>
      (?P<mantissa>
        (?P<integer1> [+-]?\d*)\.(?P<fraction>\d+)
        |
        (?P<integer2> [+-]?\d+)\.
      )
      (?:
        [Ee]
        (?P<exponent1> [+-]?\d+)
      )?
      |
      (?P<integer3> [+-]?\d+)
      (?:
        [Ee]
        (?P<exponent2> [+-]?\d+)
      )
    )
    """,
    re.VERBOSE,
)


def _init_checker_class() -> type:
    class LiteralsOutputChecker(doctest.OutputChecker):
        _number_re = _NUMBER_RE

        def check_output(self, want: str, got: str, optionflags: int) -> bool:
            if doctest.OutputChecker.check_output(self, want, got, optionflags):
                return True
            allow_unicode = optionflags & _ALLOW_UNICODE
            allow_bytes = optionflags & _ALLOW_BYTES
            allow_number = optionflags & _NUMBER
            if not (allow_unicode or allow_bytes or allow_number):
                return False
            if allow_unicode:
                want = re.sub(r"\bu'", "'", want)
                got = re.sub(r"\bu'", "'", got)
            if allow_bytes:
                want = re.sub(r"\bb'", "'", want)
                got = re.sub(r"\bb'", "'", got)
            if doctest.OutputChecker.check_output(self, want, got, optionflags):
                return True
            if allow_number:
                got = _remove_unwanted_precision(want, got)
            return doctest.OutputChecker.check_output(self, want, got, optionflags)

        def output_difference(self, example, got, optionflags):
            return doctest.OutputChecker.output_difference(self, example, got, optionflags)

    return LiteralsOutputChecker


def _remove_unwanted_precision(want: str, got: str) -> str:
    """Replace numbers in got with want's numbers when they're close enough.

    Tolerance is 10^(-precision) where precision = len(fraction) - exponent.
    Matches upstream pytest's LiteralsOutputChecker behavior.
    """
    wants = list(_NUMBER_RE.finditer(want))
    gots = list(_NUMBER_RE.finditer(got))
    if len(wants) != len(gots):
        return got
    offset = 0
    for w, g in zip(wants, gots):
        fraction = w.group("fraction")
        exponent = w.group("exponent1")
        if exponent is None:
            exponent = w.group("exponent2")
        precision = 0 if fraction is None else len(fraction)
        if exponent is not None:
            precision -= int(exponent)
        try:
            w_val = float(w.group())
            g_val = float(g.group())
        except ValueError:
            continue
        if abs(g_val - w_val) <= 10 ** (-precision):
            got = got[: g.start() + offset] + w.group() + got[g.end() + offset :]
            offset += len(w.group()) - (g.end() - g.start())
    return got


# ---------------------------------------------------------------------------
# DocTest runner
# ---------------------------------------------------------------------------


def _init_runner_class(
    continue_on_failure: bool, checker: Any, optionflags: int
) -> type[doctest.DebugRunner]:
    class PytestDoctestRunner(doctest.DebugRunner):
        def __init__(self) -> None:
            super().__init__(checker=checker, verbose=False, optionflags=optionflags)
            self._failures: list = []
            self._continue = continue_on_failure

        def report_failure(self, out, test, example, got):  # type: ignore[override]
            failure = doctest.DocTestFailure(test, example, got)
            df = DoctestFailure(test, failure, optionflags=optionflags, checker=checker)
            if self._continue:
                self._failures.append(df)
            else:
                raise df from None

        def report_unexpected_exception(self, out, test, example, exc_info):  # type: ignore[override]
            from _pytest.outcomes import OutcomeException

            if issubclass(exc_info[0], OutcomeException):
                raise exc_info[1]
            unexpected = doctest.UnexpectedException(test, example, exc_info)
            if self._continue:
                self._failures.append(unexpected)
            else:
                raise unexpected

        def run(self, test, compileflags=None, out=None, clear_globs=True):  # type: ignore[override]
            self._failures = []
            super().run(test, compileflags=compileflags, out=out, clear_globs=clear_globs)
            if self._failures:
                if len(self._failures) == 1 and isinstance(self._failures[0], DoctestFailure):
                    raise self._failures[0]
                raise MultipleDoctestFailures(self._failures)

    return PytestDoctestRunner


# ---------------------------------------------------------------------------
# Finder
# ---------------------------------------------------------------------------


def _make_finder(module: types.ModuleType) -> doctest.DocTestFinder:
    class MockAwareDocTestFinder(doctest.DocTestFinder):
        def _find(self, tests, obj, name, module, source_lines, globs, seen):
            # Skip mock objects that lack __doc__
            if hasattr(obj, "_mock_name"):
                return
            # Unwrap properties / cached_property to get docstrings.
            # Only replace with fget when it is a regular Python function;
            # if fget is a method-wrapper (overridden property), leave the
            # property as-is so the parent returns lineno=None correctly.
            if isinstance(obj, property):
                if inspect.isfunction(obj.fget):
                    obj = obj.fget
            elif isinstance(obj, cached_property):
                obj = obj.func
            try:
                super()._find(tests, obj, name, module, source_lines, globs, seen)
            except Exception:
                pass

    return MockAwareDocTestFinder(verbose=False, recurse=True)


# ---------------------------------------------------------------------------
# Skip detection
# ---------------------------------------------------------------------------


def _check_all_skipped(test: doctest.DocTest) -> None:
    all_skipped = all(x.options.get(doctest.SKIP, False) for x in test.examples)
    if test.examples and all_skipped:
        skip("all tests skipped by +SKIP option")


# ---------------------------------------------------------------------------
# Doctest func factory
# ---------------------------------------------------------------------------


def _make_doctest_func(
    dtest: doctest.DocTest,
    runner_cls: type[doctest.DebugRunner],
    optionflags: int = 0,
    module: types.ModuleType | None = None,
    extra_globs: dict | None = None,
):
    """Return a callable that runs `dtest`. Accepts doctest_namespace kwarg."""

    def run_doctest(doctest_namespace: dict | None = None, request: Any = None) -> None:
        _check_all_skipped(dtest)
        if extra_globs:
            dtest.globs.update(extra_globs)
        if doctest_namespace:
            dtest.globs.update(doctest_namespace)
        if request is not None and hasattr(request, "getfixturevalue"):
            dtest.globs["getfixture"] = request.getfixturevalue
        if module is not None:
            dtest.globs["__name__"] = module.__name__
        runner = runner_cls()
        try:
            runner.run(dtest, clear_globs=False)
        except doctest.UnexpectedException as e:
            raise DoctestUnexpected(e.test, e.example, e.exc_info, optionflags) from None

    run_doctest.__name__ = dtest.name
    return run_doctest


# ---------------------------------------------------------------------------
# Public API called from Rust
# ---------------------------------------------------------------------------


def collect_module_doctests(
    module_name: str,
    path: str,
    nodeid_base: str,
    config: Any,
    extra_globs: dict | None = None,
) -> list[tuple[str, Any, int]]:
    """Return list of (nodeid, func, lineno) for all doctests in a Python module.

    Called from Rust after collect_module() when --doctest-modules is active.
    """
    try:
        module = sys.modules.get(module_name)
        if module is None:
            spec = importlib.util.spec_from_file_location(module_name, path)
            if spec is None or spec.loader is None:
                return []
            module = importlib.util.module_from_spec(spec)
            sys.modules[module_name] = module
            with warnings.catch_warnings():
                warnings.simplefilter("ignore")
                spec.loader.exec_module(module)  # type: ignore[union-attr]
    except Exception:
        # Import errors propagate; the Rust side turns them into a skip
        # when --doctest-ignore-import-errors is set (upstream DoctestModule).
        sys.modules.pop(module_name, None)
        raise

    optionflags = get_optionflags(config)
    continue_on_failure = _get_continue_on_failure(config)
    checker = _init_checker_class()()
    runner_cls = _init_runner_class(continue_on_failure, checker, optionflags)
    finder = _make_finder(module)

    try:
        tests = finder.find(module, module.__name__)
    except Exception:
        return []

    results: list[tuple[str, Any, int]] = []
    for dtest in sorted(tests, key=lambda t: t.lineno or 0):
        if not dtest.examples:
            continue
        # nodeid: e.g. "test_foo.py::test_foo.MyClass.method"
        nodeid = f"{nodeid_base}::{dtest.name}"
        lineno = dtest.lineno or 0
        func = _make_doctest_func(
            dtest, runner_cls, optionflags=optionflags, module=module, extra_globs=extra_globs
        )
        results.append((nodeid, func, lineno))

    return results


def collect_textfile_doctests(
    path: str,
    nodeid_base: str,
    config: Any,
    extra_globs: dict | None = None,
) -> list[tuple[str, Any, int]]:
    """Return list of (nodeid, func, lineno) for doctests in a text file.

    Called from Rust for files matching --doctest-glob patterns.
    """
    optionflags = get_optionflags(config)
    continue_on_failure = _get_continue_on_failure(config)
    checker = _init_checker_class()()
    runner_cls = _init_runner_class(continue_on_failure, checker, optionflags)

    try:
        parser = doctest.DocTestParser()
        with open(path, encoding="utf-8", errors="replace") as f:
            text = f.read()
        name = os.path.basename(path)
        globs = {"__name__": "__main__"}
        if extra_globs:
            globs.update(extra_globs)
        dtest = parser.get_doctest(text, globs, name, path, 0)
    except Exception:
        return []

    if not dtest.examples:
        return []

    # Upstream's DoctestTextfile.collect() names the single DoctestItem after
    # the file's own basename (test.name = self.path.name), so the nodeid is
    # "<file>::<file>", not the bare file nodeid.
    nodeid = f"{nodeid_base}::{name}"
    func = _make_doctest_func(dtest, runner_cls, optionflags=optionflags, extra_globs=extra_globs)
    return [(nodeid, func, 0)]


def is_doctest_textfile(path: str, config: Any) -> bool:
    """Return True if path matches any --doctest-glob pattern.

    The default glob pattern (when none configured) is 'test*.txt',
    matching pytest's default behavior.
    """
    try:
        globs = config.getoption("doctest_glob") or []
    except Exception:
        globs = []
    if isinstance(globs, str):
        globs = [globs]
    if not globs:
        # Match pytest's default doctest_glob
        globs_from_ini: list = []
        try:
            ini = config.getini("doctest_glob")
            if ini:
                globs_from_ini = ini if isinstance(ini, list) else [ini]
        except Exception:
            pass
        globs = globs_from_ini or ["test*.txt"]
    basename = os.path.basename(path)
    return any(fnmatch.fnmatch(basename, g) for g in globs)


class _DoctestLineShim:
    """Carries the lineno that DoctestItem.reportinfo needs when an item is
    rebuilt in-process (pytester.inline_genitems) without the full DocTest."""

    def __init__(self, lineno: int) -> None:
        self.lineno = lineno
        self.examples = [None]


def inprocess_doctest_items(
    path: str,
    config: Any,
    doctest_modules: bool,
    glob_patterns: list,
    nodeid_base: str,
) -> list:
    """Collect a file's doctests in-process as real DoctestItem objects (with a
    DoctestModule/DoctestTextfile parent and a reportinfo lineno), for
    pytester.inline_genitems. Returns [] when doctest collection doesn't apply
    to `path` (a .py file without --doctest-modules, or a text file matching no
    --doctest-glob pattern)."""
    basename = os.path.basename(path)
    if path.endswith(".py"):
        if not doctest_modules:
            return []
        module_name = os.path.splitext(basename)[0]
        try:
            results = collect_module_doctests(module_name, path, nodeid_base, config)
        except Exception:
            return []
        parent: Any = DoctestModule(path, config)
    elif any(fnmatch.fnmatch(basename, pat) for pat in glob_patterns):
        results = collect_textfile_doctests(path, nodeid_base, config)
        parent = DoctestTextfile(path, config)
    else:
        return []

    items = []
    for nodeid, _func, lineno in results:
        item = DoctestItem(nodeid, _DoctestLineShim(lineno))  # type: ignore[arg-type]
        item._pytest_doctest_item = True  # type: ignore[attr-defined]
        item.nodeid = nodeid  # type: ignore[attr-defined]
        item.parent = parent  # type: ignore[attr-defined]
        items.append(item)
    return items


def _get_ignore_import_errors(config: Any) -> bool:
    try:
        return bool(config.getoption("doctest_ignore_import_errors"))
    except Exception:
        return False


# ---------------------------------------------------------------------------
# Compatibility shims for code that imports internal pytest names
# ---------------------------------------------------------------------------


def _get_checker():
    """Return the output checker class (pytest public internal)."""
    return _init_checker_class()()


def _is_main_py(path: str) -> bool:
    return os.path.basename(path) == "__main__.py"


def _is_setup_py(path: str) -> bool:
    return os.path.basename(path) == "setup.py"


def _is_mocked(obj: Any) -> bool:
    return hasattr(obj, "_mock_name") or hasattr(obj, "_mock_methods")


def _patch_unwrap_mock_aware():
    """Context manager that patches inspect.unwrap to skip mock objects
    and warn (then re-raise) when a broken object explodes during unwrap."""

    @contextlib.contextmanager
    def _ctx():
        real_unwrap = inspect.unwrap

        def _mock_aware_unwrap(func, *, stop=None):
            from pytest._warning_types import PytestWarning

            try:
                if stop is None or stop is _is_mocked:
                    return real_unwrap(func, stop=_is_mocked)
                _stop = stop
                return real_unwrap(func, stop=lambda obj: _is_mocked(obj) or _stop(func))
            except Exception as e:
                warnings.warn(
                    f"Got {e!r} when unwrapping {func!r}.  This is usually caused "
                    "by a violation of Python's object protocol; see e.g. "
                    "https://github.com/pytest-dev/pytest/issues/5080",
                    PytestWarning,
                )
                raise

        inspect.unwrap = _mock_aware_unwrap
        try:
            yield
        finally:
            inspect.unwrap = real_unwrap

    return _ctx()


class _DoctestItemMeta(type):
    """Metaclass that makes isinstance(item, DoctestItem) work for pytest-rs node proxies."""

    def __instancecheck__(cls, instance: Any) -> bool:
        return getattr(instance, "_pytest_doctest_item", False)


class DoctestItem(metaclass=_DoctestItemMeta):
    """Stub for pytest DoctestItem; isinstance works for DoctestNode proxies."""

    def __init__(self, name: str, dtest: doctest.DocTest) -> None:
        self.name = name
        self.dtest = dtest

    def reportinfo(self) -> tuple:
        if self.dtest is not None:
            lineno = self.dtest.lineno
        else:
            lineno = self._compute_lineno()
        filename = self.name.split("::")[0] if "::" in self.name else self.name
        name_part = self.name.split("::", 1)[1] if "::" in self.name else self.name
        return filename, lineno, f"[doctest] {name_part}"

    def _compute_lineno(self) -> int | None:
        if not hasattr(self, "_pytester_path") or "::" not in self.name:
            return None
        relpath, dotname = self.name.split("::", 1)
        abs_path = str(self._pytester_path / relpath)
        # Qualified name within module (strip the module-name prefix from dotname).
        parts = dotname.split(".", 1)
        qualified = parts[1] if len(parts) > 1 else parts[0]
        try:
            spec = importlib.util.spec_from_file_location("_ri_tmp_", abs_path)
            if spec is None or spec.loader is None:
                return None
            mod = types.ModuleType("_ri_tmp_")
            spec.loader.exec_module(mod)  # type: ignore[union-attr]
            for t in doctest.DocTestFinder().find(mod):
                t_qualified = t.name.split(".", 1)[1] if "." in t.name else t.name
                if t_qualified == qualified:
                    return t.lineno
        except Exception:
            pass
        return None

    def repr_failure(self, excinfo: Any) -> str:
        exc = excinfo.value if hasattr(excinfo, "value") else excinfo
        if isinstance(exc, DoctestFailure):
            return exc.msg
        return str(exc)


class DoctestModule:
    """Minimal stub for DoctestModule (pytest internal class)."""

    def __init__(self, fspath: Any, config: Any) -> None:
        self.fspath = fspath
        self.config = config


class DoctestTextfile:
    """Minimal stub for DoctestTextfile (pytest internal class)."""

    def __init__(self, fspath: Any, config: Any) -> None:
        self.fspath = fspath
        self.config = config
