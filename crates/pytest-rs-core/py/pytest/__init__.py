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
