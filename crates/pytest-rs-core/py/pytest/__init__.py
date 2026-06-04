"""pytest API shim provided by pytest-rs.

Decorators only *record* metadata on functions (exactly like real pytest);
the Rust engine introspects the imported module afterwards. Nothing here
resolves fixtures or runs tests.
"""

import re as _re

__version__ = "8.3.4"  # pytest API version this shim tracks
__pytest_rs__ = True


# ---------------------------------------------------------------------------
# outcomes
# ---------------------------------------------------------------------------


class OutcomeException(BaseException):
    def __init__(self, msg=None):
        super().__init__(msg)
        self.msg = msg


class Skipped(OutcomeException):
    pass


class Failed(OutcomeException):
    pass


class XFailed(Failed):
    pass


def skip(reason=""):
    raise Skipped(msg=reason)


def fail(reason="", pytrace=True):
    raise Failed(msg=reason)


def xfail(reason=""):
    raise XFailed(msg=reason)


def importorskip(modname, minversion=None, reason=None):
    import importlib

    try:
        mod = importlib.import_module(modname)
    except ImportError:
        raise Skipped(msg=reason or f"could not import {modname!r}") from None
    if minversion is not None:
        version = getattr(mod, "__version__", None)
        if version is None or version < minversion:
            raise Skipped(
                msg=f"module {modname!r} has __version__ {version}, required is: {minversion!r}"
            )
    return mod


# ---------------------------------------------------------------------------
# fixtures
# ---------------------------------------------------------------------------


class FixtureFunctionMarker:
    def __init__(self, scope="function", params=None, autouse=False, ids=None, name=None):
        self.scope = scope
        self.params = list(params) if params is not None else None
        self.autouse = autouse
        self.ids = ids
        self.name = name

    def __call__(self, function):
        function._pytestfixturefunction = self
        return function


def fixture(
    fixture_function=None, *, scope="function", params=None, autouse=False, ids=None, name=None
):
    marker = FixtureFunctionMarker(scope=scope, params=params, autouse=autouse, ids=ids, name=name)
    if fixture_function is not None:
        return marker(fixture_function)
    return marker


# ---------------------------------------------------------------------------
# marks
# ---------------------------------------------------------------------------


class Mark:
    def __init__(self, name, args=(), kwargs=None):
        self.name = name
        self.args = tuple(args)
        self.kwargs = dict(kwargs or {})

    def __repr__(self):
        return f"Mark({self.name!r}, {self.args!r}, {self.kwargs!r})"


class MarkDecorator:
    def __init__(self, mark):
        self.mark = mark

    @property
    def name(self):
        return self.mark.name

    def __call__(self, *args, **kwargs):
        if len(args) == 1 and not kwargs and (callable(args[0]) or isinstance(args[0], type)):
            func = args[0]
            existing = list(getattr(func, "pytestmark", []))
            existing.append(self.mark)
            func.pytestmark = existing
            return func
        return MarkDecorator(
            Mark(self.mark.name, self.mark.args + args, {**self.mark.kwargs, **kwargs})
        )


class MarkGenerator:
    def __getattr__(self, name):
        if name.startswith("_"):
            raise AttributeError(name)
        return MarkDecorator(Mark(name))


mark = MarkGenerator()


class ParamSpec:
    """The object returned by pytest.param(): values + per-param marks/id."""

    def __init__(self, values, marks, id):
        self.values = tuple(values)
        self.marks = list(marks)
        self.id = id


def param(*values, marks=(), id=None):
    if not isinstance(marks, list | tuple):
        marks = [marks]
    return ParamSpec(values, [decorator.mark for decorator in marks], id)


# ---------------------------------------------------------------------------
# raises
# ---------------------------------------------------------------------------


class ExceptionInfo:
    def __init__(self):
        self.type = None
        self.value = None
        self.tb = None

    def _set(self, type_, value, tb):
        self.type = type_
        self.value = value
        self.tb = tb

    @property
    def typename(self):
        return self.type.__name__ if self.type else None

    def match(self, regexp):
        if not _re.search(regexp, str(self.value)):
            fail(f"Regex pattern did not match.\n Regex: {regexp!r}\n Input: {str(self.value)!r}")
        return True


class RaisesContext:
    def __init__(self, expected_exception, match=None):
        self.expected_exception = expected_exception
        self.match_expr = match
        self.excinfo = None

    def __enter__(self):
        self.excinfo = ExceptionInfo()
        return self.excinfo

    def __exit__(self, exc_type, exc_value, tb):
        if exc_type is None:
            expected = getattr(self.expected_exception, "__name__", str(self.expected_exception))
            fail(f"DID NOT RAISE {expected}")
        if not issubclass(exc_type, self.expected_exception):
            return False
        self.excinfo._set(exc_type, exc_value, tb)
        if self.match_expr is not None:
            self.excinfo.match(self.match_expr)
        return True


def raises(expected_exception, *args, match=None, **kwargs):
    if args:
        func, *fargs = args
        with RaisesContext(expected_exception) as excinfo:
            func(*fargs, **kwargs)
        return excinfo
    return RaisesContext(expected_exception, match=match)


# ---------------------------------------------------------------------------
# approx (numeric subset; sequences/dicts of numbers)
# ---------------------------------------------------------------------------


class _Approx:
    DEFAULT_REL = 1e-6
    DEFAULT_ABS = 1e-12

    def __init__(self, expected, rel=None, abs=None):
        self.expected = expected
        self.rel = rel
        self.abs = abs

    def _eq_scalar(self, actual, expected):
        if expected == actual:
            return True
        abs_tol = self.abs if self.abs is not None else self.DEFAULT_ABS
        rel_tol = self.rel if self.rel is not None else self.DEFAULT_REL
        return abs(actual - expected) <= max(abs_tol, rel_tol * abs(expected))

    def __eq__(self, actual):
        expected = self.expected
        if isinstance(expected, dict):
            return (
                isinstance(actual, dict)
                and actual.keys() == expected.keys()
                and all(self._eq_scalar(actual[k], expected[k]) for k in expected)
            )
        if isinstance(expected, (list, tuple)):
            return len(actual) == len(expected) and all(
                self._eq_scalar(a, e) for a, e in zip(actual, expected, strict=False)
            )
        return self._eq_scalar(actual, expected)

    def __ne__(self, actual):
        return not (self == actual)

    def __repr__(self):
        return f"approx({self.expected!r})"


def approx(expected, rel=None, abs=None, nan_ok=False):
    return _Approx(expected, rel=rel, abs=abs)


# ---------------------------------------------------------------------------
# monkeypatch
# ---------------------------------------------------------------------------


class MonkeyPatch:
    def __init__(self):
        self._setattr = []
        self._setitem = []
        self._cwd = None
        self._savesyspath = None

    @classmethod
    def context(cls):
        import contextlib

        @contextlib.contextmanager
        def _context():
            m = cls()
            try:
                yield m
            finally:
                m.undo()

        return _context()

    _notset = object()

    def setattr(self, target, name, value=_notset, raising=True):
        if isinstance(target, str):
            # setattr("module.path.attr", value) form
            import importlib

            if value is self._notset:
                value = name
                module_path, _, name = target.rpartition(".")
            else:
                module_path, _, name = target.rpartition(".")
            target = importlib.import_module(module_path)
        elif value is self._notset:
            raise TypeError("setattr requires a value when target is an object")
        old = getattr(target, name, self._notset)
        if raising and old is self._notset:
            raise AttributeError(f"{target!r} has no attribute {name!r}")
        self._setattr.append((target, name, old))
        setattr(target, name, value)

    def delattr(self, target, name=_notset, raising=True):
        if isinstance(target, str):
            import importlib

            module_path, _, attr_name = target.rpartition(".")
            target = importlib.import_module(module_path)
            name = attr_name
        old = getattr(target, name, self._notset)
        if old is self._notset:
            if raising:
                raise AttributeError(name)
            return
        self._setattr.append((target, name, old))
        delattr(target, name)

    def setitem(self, mapping, name, value):
        self._setitem.append((mapping, name, mapping.get(name, self._notset)))
        mapping[name] = value

    def delitem(self, mapping, name, raising=True):
        if name not in mapping:
            if raising:
                raise KeyError(name)
            return
        self._setitem.append((mapping, name, mapping[name]))
        del mapping[name]

    def setenv(self, name, value, prepend=None):
        import os

        value = str(value)
        if prepend and name in os.environ:
            value = value + prepend + os.environ[name]
        self.setitem(os.environ, name, value)

    def delenv(self, name, raising=True):
        import os

        self.delitem(os.environ, name, raising=raising)

    def syspath_prepend(self, path):
        import sys

        if self._savesyspath is None:
            self._savesyspath = sys.path[:]
        sys.path.insert(0, str(path))

    def chdir(self, path):
        import os

        if self._cwd is None:
            self._cwd = os.getcwd()
        os.chdir(path)

    def undo(self):
        import os
        import sys

        for target, name, old in reversed(self._setattr):
            if old is self._notset:
                try:
                    delattr(target, name)
                except AttributeError:
                    pass
            else:
                setattr(target, name, old)
        self._setattr.clear()
        for mapping, name, old in reversed(self._setitem):
            if old is self._notset:
                mapping.pop(name, None)
            else:
                mapping[name] = old
        self._setitem.clear()
        if self._savesyspath is not None:
            sys.path[:] = self._savesyspath
            self._savesyspath = None
        if self._cwd is not None:
            os.chdir(self._cwd)
            self._cwd = None


@fixture
def monkeypatch():
    m = MonkeyPatch()
    yield m
    m.undo()


# ---------------------------------------------------------------------------
# tmp_path
# ---------------------------------------------------------------------------


@fixture
def tmp_path():
    import pathlib
    import shutil
    import tempfile

    path = pathlib.Path(tempfile.mkdtemp(prefix="pytest-rs-tmp-"))
    yield path
    shutil.rmtree(path, ignore_errors=True)


# ---------------------------------------------------------------------------
# pytester: run pytest-rs as a child process so upstream test suites can
# exercise the runner itself.
# ---------------------------------------------------------------------------

_OUTCOME_RE = None
_ANSI_RE = None


def _outcome_regexes():
    global _OUTCOME_RE, _ANSI_RE
    if _OUTCOME_RE is None:
        _OUTCOME_RE = _re.compile(
            r"(\d+) (passed|failed|skipped|xfailed|xpassed|errors?|warnings?|deselected)"
        )
        _ANSI_RE = _re.compile(r"\x1b\[[0-9;]*m")
    return _OUTCOME_RE, _ANSI_RE


class LineMatcher:
    def __init__(self, lines):
        self.lines = lines

    def __str__(self):
        return "\n".join(self.lines)

    def str(self):
        return str(self)

    def fnmatch_lines(self, patterns):
        import fnmatch

        if isinstance(patterns, str):
            patterns = [patterns]
        remaining = list(self.lines)
        for pattern in patterns:
            for index, line in enumerate(remaining):
                if fnmatch.fnmatch(line, pattern):
                    remaining = remaining[index + 1 :]
                    break
            else:
                fail(f"fnmatch_lines: no line matches {pattern!r} in:\n{self}")

    def no_fnmatch_line(self, pattern):
        import fnmatch

        for line in self.lines:
            if fnmatch.fnmatch(line, pattern):
                fail(f"no_fnmatch_line: unexpectedly matched {pattern!r}: {line!r}")


class RunResult:
    def __init__(self, ret, outlines, errlines, duration):
        self.ret = ret
        self.outlines = outlines
        self.errlines = errlines
        self.duration = duration
        self.stdout = LineMatcher(outlines)
        self.stderr = LineMatcher(errlines)

    def parseoutcomes(self):
        outcome_re, ansi_re = _outcome_regexes()
        for line in reversed(self.outlines):
            clean = ansi_re.sub("", line)
            if clean.startswith("====") and " in " in clean:
                found = {}
                for count, key in outcome_re.findall(clean):
                    found[key.rstrip("s") if key in ("errors", "warnings") else key] = int(count)
                return found
        return {}

    def assert_outcomes(
        self,
        passed=0,
        skipped=0,
        failed=0,
        errors=0,
        xpassed=0,
        xfailed=0,
        warnings=None,
        deselected=None,
    ):
        actual = self.parseoutcomes()
        expected = {
            "passed": passed,
            "skipped": skipped,
            "failed": failed,
            "error": errors,
            "xpassed": xpassed,
            "xfailed": xfailed,
        }
        got = {key: actual.get(key, 0) for key in expected}
        assert got == expected, f"assert_outcomes: expected {expected}, got {actual}"
        if warnings is not None:
            assert actual.get("warning", 0) == warnings
        if deselected is not None:
            assert actual.get("deselected", 0) == deselected


class Pytester:
    def __init__(self, path, request_name):
        import pathlib

        self.path = pathlib.Path(path)
        self._name = request_name

    def _makefile(self, ext, args, kwargs):
        items = list(kwargs.items())
        if args:
            source = "\n".join(str(arg) for arg in args)
            items.insert(0, (self._name, source))
        paths = []
        for basename, source in items:
            import textwrap

            path = self.path / (basename + ext)
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(textwrap.dedent(str(source)).lstrip("\n"))
            paths.append(path)
        return paths[0] if len(paths) == 1 else paths

    def makepyfile(self, *args, **kwargs):
        return self._makefile(".py", args, kwargs)

    def makeconftest(self, source):
        return self._makefile(".py", [], {"conftest": source})

    def maketxtfile(self, *args, **kwargs):
        return self._makefile(".txt", args, kwargs)

    def makeini(self, source):
        return self._makefile(".ini", [], {"tox": source})

    def makefile(self, ext, *args, **kwargs):
        return self._makefile(ext, args, kwargs)

    def mkdir(self, name):
        path = self.path / name
        path.mkdir()
        return path

    def runpytest(self, *args):
        import os
        import subprocess
        import time

        exe = os.environ.get("PYTEST_RS_EXE")
        if exe is None:
            fail("PYTEST_RS_EXE is not set; pytester cannot run the runner")
        start = time.perf_counter()
        proc = subprocess.run(
            [exe, *[str(arg) for arg in args]],
            cwd=self.path,
            capture_output=True,
            text=True,
            timeout=120,
        )
        duration = time.perf_counter() - start
        _, ansi_re = _outcome_regexes()
        outlines = [ansi_re.sub("", line) for line in proc.stdout.splitlines()]
        errlines = [ansi_re.sub("", line) for line in proc.stderr.splitlines()]
        return RunResult(proc.returncode, outlines, errlines, duration)

    runpytest_subprocess = runpytest
    runpytest_inprocess = runpytest


@fixture
def pytester(request):
    import os
    import re
    import shutil
    import tempfile

    name = re.sub(r"\W", "_", request.node.name)
    path = tempfile.mkdtemp(prefix="pytest-rs-pytester-")
    old_cwd = os.getcwd()
    os.chdir(path)
    yield Pytester(path, name)
    os.chdir(old_cwd)
    shutil.rmtree(path, ignore_errors=True)
