"""Doctest collection and execution for pytest-rs.

Called from Rust after module collection when --doctest-modules is active,
and for text files matching --doctest-glob patterns.
"""
from __future__ import annotations

import doctest
import os
import re
import sys
import types
import warnings
from functools import cached_property
from typing import Any, Iterable, List, Optional, Tuple

import pytest
from _pytest.outcomes import skip, fail


# ---------------------------------------------------------------------------
# Exceptions
# ---------------------------------------------------------------------------

class DoctestFailure(Exception):
    """Raised when a doctest example fails; formatted without a Python traceback."""

    pytrace = False

    def __init__(self, dtest: doctest.DocTest, failure: doctest.DocTestFailure) -> None:
        self.dtest = dtest
        self.failure = failure
        super().__init__(str(failure))

    @property
    def msg(self) -> str:
        return self._format()

    def _format(self) -> str:
        lines = ["FAILED: " + self.dtest.name]
        ex = self.failure.example
        lines.append(f"Failed example:\n    {ex.source.rstrip()}")
        if self.failure.got is not None:
            lines.append(f"Expected:\n    {(ex.want or '(nothing)').rstrip()}")
            lines.append(f"Got:\n    {self.failure.got.rstrip()}")
        return "\n".join(lines)


class MultipleDoctestFailures(Exception):
    """Raised when continue_on_failure=True and multiple examples failed."""

    pytrace = False

    def __init__(self, failures: list) -> None:
        self.failures = failures
        super().__init__(f"{len(failures)} doctest failure(s)")

    @property
    def msg(self) -> str:
        return "\n\n".join(f._format() for f in self.failures)


# ---------------------------------------------------------------------------
# Option flags
# ---------------------------------------------------------------------------

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
    ini_flags = config.getini("doctest_optionflags") if hasattr(config, "getini") else []
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

# Attempt to use pytest's built-in flags if available (they live in doctest at
# high bit positions). Define them locally so we can always reference them.
_ALLOW_UNICODE = 0
_ALLOW_BYTES = 0
_NUMBER = 0

for _name, _bits in [("ALLOW_UNICODE", 1 << 16), ("ALLOW_BYTES", 1 << 17), ("NUMBER", 1 << 18)]:
    if not hasattr(doctest, _name):
        setattr(doctest, _name, _bits)
        doctest.OPTIONFLAGS_BY_NAME[_name] = _bits
    exec(f"_{_name} = doctest.{_name}")

_NUMBER_RE = re.compile(
    r"(?:[-+]?(?:(?:\d[\d_]*)?\.(?:\d[\d_]*)|\d[\d_]*\.)(?:[eE][-+]?\d+)?|[-+]?\d[\d_]*[eE][-+]?\d+)"
)


def _init_checker_class() -> type:
    class LiteralsOutputChecker(doctest.OutputChecker):
        _number_re = _NUMBER_RE

        def check_output(self, want: str, got: str, optionflags: int) -> bool:
            if doctest.OutputChecker.check_output(self, want, got, optionflags):
                return True
            if optionflags & _ALLOW_UNICODE:
                want2 = re.sub(r"\bu'", "'", want)
                got2 = re.sub(r"\bu'", "'", got)
                if doctest.OutputChecker.check_output(self, want2, got2, optionflags):
                    return True
            if optionflags & _ALLOW_BYTES:
                want2 = re.sub(r"\bb'", "'", want)
                got2 = re.sub(r"\bb'", "'", got)
                if doctest.OutputChecker.check_output(self, want2, got2, optionflags):
                    return True
            if optionflags & _NUMBER:
                if _check_number(want, got, optionflags):
                    return True
            return False

        def output_difference(self, example, got, optionflags):
            return doctest.OutputChecker.output_difference(self, example, got, optionflags)

    return LiteralsOutputChecker


def _check_number(want: str, got: str, optionflags: int) -> bool:
    want_nums = _NUMBER_RE.findall(want)
    got_nums = _NUMBER_RE.findall(got)
    if len(want_nums) != len(got_nums):
        return False
    want_rest = _NUMBER_RE.sub("", want).strip()
    got_rest = _NUMBER_RE.sub("", got).strip()
    if want_rest != got_rest:
        return False
    for wn, gn in zip(want_nums, got_nums):
        try:
            if abs(float(wn) - float(gn)) > 1e-6 * max(1, abs(float(wn))):
                return False
        except ValueError:
            if wn != gn:
                return False
    return True


# ---------------------------------------------------------------------------
# DocTest runner
# ---------------------------------------------------------------------------

def _init_runner_class(continue_on_failure: bool, checker: Any, optionflags: int) -> doctest.DebugRunner:
    class PytestDoctestRunner(doctest.DebugRunner):
        def __init__(self) -> None:
            super().__init__(checker=checker, verbose=False, optionflags=optionflags)
            self._failures: list = []
            self._continue = continue_on_failure

        def report_failure(self, out, test, example, got):  # type: ignore[override]
            failure = doctest.DocTestFailure(test, example, got)
            df = DoctestFailure(test, failure)
            if self._continue:
                self._failures.append(df)
            else:
                raise df from None

        def report_unexpected_exception(self, out, test, example, exc_info):  # type: ignore[override]
            if issubclass(exc_info[0], (skip.Exception,)):
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
            # Unwrap properties / cached_property to get docstrings
            if isinstance(obj, property):
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
    if not test.examples:
        skip(f"no examples in doctest: {test.name}")
    all_skip = all(
        doctest.SKIP & ex.options.get(doctest.SKIP, 0) or "+SKIP" in ex.source
        for ex in test.examples
    )
    if all_skip:
        skip(f"all examples skipped in doctest: {test.name}")


# ---------------------------------------------------------------------------
# Doctest func factory
# ---------------------------------------------------------------------------

def _make_doctest_func(
    dtest: doctest.DocTest,
    runner_cls: type,
    module: Optional[types.ModuleType] = None,
    extra_globs: Optional[dict] = None,
):
    """Return a callable that runs `dtest`. Accepts doctest_namespace kwarg."""

    def run_doctest(doctest_namespace: Optional[dict] = None, request: Any = None) -> None:
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
            raise e.exc_info[1].with_traceback(e.exc_info[2]) from None

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
    extra_globs: Optional[dict] = None,
) -> List[Tuple[str, Any, int]]:
    """Return list of (nodeid, func, lineno) for all doctests in a Python module.

    Called from Rust after collect_module() when --doctest-modules is active.
    """
    try:
        module = sys.modules.get(module_name)
        if module is None:
            import importlib.util
            spec = importlib.util.spec_from_file_location(module_name, path)
            if spec is None or spec.loader is None:
                return []
            module = importlib.util.module_from_spec(spec)
            sys.modules[module_name] = module
            with warnings.catch_warnings():
                warnings.simplefilter("ignore")
                spec.loader.exec_module(module)  # type: ignore[union-attr]
    except Exception:
        if _get_ignore_import_errors(config):
            return []
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

    results: List[Tuple[str, Any, int]] = []
    for dtest in sorted(tests, key=lambda t: t.lineno or 0):
        if not dtest.examples:
            continue
        # nodeid: e.g. "test_foo.py::test_foo.MyClass.method"
        nodeid = f"{nodeid_base}::{dtest.name}"
        lineno = dtest.lineno or 0
        func = _make_doctest_func(dtest, runner_cls, module=module, extra_globs=extra_globs)
        results.append((nodeid, func, lineno))

    return results


def collect_textfile_doctests(
    path: str,
    nodeid_base: str,
    config: Any,
    extra_globs: Optional[dict] = None,
) -> List[Tuple[str, Any, int]]:
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

    nodeid = nodeid_base
    func = _make_doctest_func(dtest, runner_cls, extra_globs=extra_globs)
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
    import fnmatch
    basename = os.path.basename(path)
    return any(fnmatch.fnmatch(basename, g) for g in globs)


def _get_ignore_import_errors(config: Any) -> bool:
    try:
        return bool(config.getoption("doctest_ignore_import_errors"))
    except Exception:
        return False


# ---------------------------------------------------------------------------
# doctest_namespace fixture (session-scoped, injected by Rust)
# ---------------------------------------------------------------------------

@pytest.fixture(scope="session")
def doctest_namespace() -> dict:
    """Fixture providing a namespace dict injected into all doctests."""
    return {}


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
    """Context manager that patches inspect.unwrap to skip mock objects."""
    import contextlib, inspect

    @contextlib.contextmanager
    def _ctx():
        original = inspect.unwrap

        def unwrap_safe(obj, *, stop=None):
            if _is_mocked(obj):
                return obj
            try:
                return original(obj, stop=stop)
            except Exception:
                return obj

        inspect.unwrap = unwrap_safe
        try:
            yield
        finally:
            inspect.unwrap = original

    return _ctx()


class DoctestItem:
    """Minimal pytest DoctestItem stub for introspection by conformance tests."""

    def __init__(self, name: str, dtest: doctest.DocTest) -> None:
        self.name = name
        self.dtest = dtest

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
