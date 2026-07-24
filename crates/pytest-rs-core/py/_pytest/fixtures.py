import functools
import inspect
import sys
from collections.abc import Sequence
from pathlib import Path
from typing import Any

import pytest

# Set on the pytest module by the Rust engine at startup (a pyo3 class).
FixtureRequest = getattr(pytest, "FixtureRequest", object)


from pytest._fixtures import (  # noqa: E402
    FixtureFunctionDefinition as _RealFixtureFunctionDefinition,
)
from pytest._fixtures import FixtureLookupError as FixtureLookupError  # noqa: E402


@pytest.fixture(scope="session")
def pytestconfig(request):
    """Session-scoped fixture that returns the session's :class:`pytest.Config` object."""
    return request.config


def getfixturemarker(obj):
    """Return the fixture's FixtureFunctionMarker, or None if it isn't a fixture."""
    if isinstance(obj, _RealFixtureFunctionDefinition):
        return obj._fixture_function_marker
    return None


class FuncFixtureInfo:
    """Fixture-related information for a fixture-requesting item (e.g. test function).

    Mirrors upstream _pytest.fixtures.FuncFixtureInfo. Plugins like anyio use
    dataclasses.fields(CallSpec2) to detect pytest >= 8 and then access
    item._fixtureinfo to build new items with modified fixture closures.
    """

    __slots__ = ("argnames", "initialnames", "name2fixturedefs", "names_closure")

    def __init__(
        self,
        argnames: tuple[str, ...] = (),
        initialnames: tuple[str, ...] = (),
        names_closure: list[str] | None = None,
        name2fixturedefs: dict[str, Sequence[Any]] | None = None,
    ) -> None:
        self.argnames: tuple[str, ...] = tuple(argnames)
        self.initialnames: tuple[str, ...] = tuple(initialnames)
        self.names_closure: list[str] = list(names_closure) if names_closure is not None else []
        self.name2fixturedefs: dict[str, Sequence[Any]] = (
            dict(name2fixturedefs) if name2fixturedefs is not None else {}
        )


class _Subscriptable:
    """A real, subscriptable class so `FixtureDef[Any]` in type expressions
    (pytest-bdd casts) evaluates without error. pytest-rs never instantiates
    these — they exist for annotations/isinstance compatibility."""

    def __class_getitem__(cls, item):
        return cls


class FixtureDef(_Subscriptable):
    """Annotation/typing stand-in for _pytest.fixtures.FixtureDef. pytest-rs
    exposes its own ShimFixtureDef objects through request._fixturemanager."""


class FixtureManager(_Subscriptable):
    """Annotation/typing stand-in for _pytest.fixtures.FixtureManager.
    Accepts a session so test scaffolding (Session._fixturemanager =
    FixtureManager(session)) constructs without error."""

    def __init__(self, session=None):
        self.session = session
        self.config = getattr(session, "config", None)


def deduplicate_names(*seqs):
    """De-duplicate the sequence of names while keeping the original order."""
    return tuple(dict.fromkeys(name for seq in seqs for name in seq))


class TopRequest:
    """Minimal stand-in for `_pytest.fixtures.TopRequest`: wraps a collected
    Function node so tests can inspect `request.fixturenames` / `.path` from a
    statically-collected item (e.g. inline_genitems) without a live run."""

    def __init__(self, pyfuncitem, *, _ispytest=False):
        self._pyfuncitem = pyfuncitem
        self._fixture_defs = {}
        # Full fixturedef map (name -> (scope, argnames, func, owning_cls)) for
        # in-process resolution; empty for statically-collected items that only
        # expose closure/metadata.
        self._fixturedefs_full = getattr(pyfuncitem, "_fixturedefs_full", {}) or {}
        self._fixture_values = {}
        self._finalizers = []

    # The fixture this (sub)request resolves for; TopRequest is the test's own
    # request, so there is none. call_fixture_func only reads it for an error.
    fixturename = None

    @property
    def node(self):
        return self._pyfuncitem

    @property
    def fixturenames(self):
        result = list(self._pyfuncitem.fixturenames)
        result.extend(set(self._fixture_defs).difference(result))
        return result

    @property
    def path(self):
        return self._pyfuncitem.path

    @property
    def module(self):
        return getattr(self._pyfuncitem, "module", None)

    @property
    def cls(self):
        return getattr(self._pyfuncitem, "cls", None)

    @property
    def function(self):
        return getattr(self._pyfuncitem, "obj", None)

    @property
    def instance(self):
        return getattr(self._pyfuncitem, "instance", None)

    @property
    def keywords(self):
        return self._pyfuncitem.keywords

    @property
    def config(self):
        return getattr(self._pyfuncitem, "config", None)

    @property
    def _arg2fixturedefs(self):
        """Map each requested fixture name (excluding the builtin `request`) to
        a single-element list of a fixturedef-like object carrying `.argname`,
        mirroring _pytest.fixtures.FixtureRequest._arg2fixturedefs for a
        statically collected item."""
        result = {}
        for name in self._pyfuncitem.fixturenames:
            if name == "request":
                continue
            result[name] = [_ShimArgFixtureDef(name)]
        return result

    def _resolve(self, name):
        """Resolve a fixture by name in-process, caching its value. Recurses
        into dependencies; `request` resolves to this request."""
        if name == "request":
            return self
        if name in self._fixture_values:
            return self._fixture_values[name]
        info = self._fixturedefs_full.get(name)
        if info is None:
            from pytest._fixtures import FixtureLookupError

            raise FixtureLookupError(f"fixture {name!r} not found")
        _scope, argnames, func, owning_cls = info
        kwargs = {dep: self._resolve(dep) for dep in argnames}
        if owning_cls is not None:
            instance = getattr(self._pyfuncitem, "instance", None)
            if instance is None or isinstance(instance, type):
                try:
                    instance = owning_cls()
                except Exception:
                    instance = owning_cls
            func = func.__get__(instance, owning_cls)
        value = call_fixture_func(func, self, kwargs)
        self._fixture_values[name] = value
        return value

    def getfixturevalue(self, argname):
        """Dynamically resolve a fixture (pytest's request.getfixturevalue)."""
        return self._resolve(argname)

    def _fillfixtures(self):
        """Populate item.funcargs with the test's fixture closure + request,
        mirroring pytest's Function.setup -> request._fillfixtures."""
        funcargs = getattr(self._pyfuncitem, "funcargs", None)
        if funcargs is None:
            funcargs = {}
            self._pyfuncitem.funcargs = funcargs
        for name in self._pyfuncitem.fixturenames:
            if name == "request":
                continue
            if name in self._fixturedefs_full:
                funcargs[name] = self._resolve(name)
        funcargs["request"] = self

    def addfinalizer(self, finalizer):
        """Register a teardown callback. Routes to the item's SetupState when
        the item has been set up (so SetupState.teardown_exact drains it),
        else keeps it locally."""
        item = self._pyfuncitem
        session = getattr(item, "session", None)
        setupstate = getattr(session, "_setupstate", None) if session is not None else None
        if setupstate is not None and item in getattr(setupstate, "stack", {}):
            setupstate.addfinalizer(finalizer, item)
        else:
            self._finalizers.append(finalizer)

    def applymarker(self, marker):
        """Apply a marker to the underlying test item (pytest's
        request.applymarker → node.add_marker): validate it and make it visible
        in item.keywords. Append straight to own_markers rather than going
        through Node.add_marker, whose global add-marks recording would leak the
        mark onto the live test that built this static item."""
        from pytest._marks import Mark, MarkDecorator

        if isinstance(marker, str):
            marker = Mark(marker)
        elif isinstance(marker, MarkDecorator):
            marker = marker.mark
        else:
            raise ValueError(f"{marker!r} is not a Mark or MarkDecorator")
        self._pyfuncitem.own_markers.append(marker)

    def __repr__(self):
        return f"<FixtureRequest for {self._pyfuncitem!r}>"


class _ShimArgFixtureDef:
    """A minimal fixturedef stand-in exposing `.argname`, returned by
    TopRequest._arg2fixturedefs for statically collected items."""

    def __init__(self, argname):
        self.argname = argname


def raise_did_not_yield(fixturename):
    """Raise ValueError for a yield fixture that yielded no value.

    Raised from Python (not constructed in Rust) so the exception carries a
    ``__traceback__``, enabling pytest's E-prefix traceback rendering.

    The ``raise`` is nested to column 12 to mirror upstream
    _pytest/fixtures.py:910: the traceback repr aligns the E-line as
    ``"E" + " " * (3 + source_indent)``, so keeping the same column makes the
    rendered E-prefix identical to pytest's output.
    """
    if True:  # noqa: SIM102 - nesting is intentional, for E-indent alignment
        if True:  # noqa: SIM102
            raise ValueError(f"{fixturename} did not yield a value") from None


def call_fixture_func(fixturefunc, request, kwargs):
    """Call a fixture-style function, honoring a single `yield` for teardown
    (pytest-bdd runs step functions through this so steps may yield). Mirrors
    _pytest.fixtures.call_fixture_func for the sync case."""
    if inspect.isgeneratorfunction(fixturefunc):
        generator = fixturefunc(**kwargs)
        try:
            value = next(generator)
        except StopIteration:
            raise_did_not_yield(request.fixturename)
        request.addfinalizer(functools.partial(_teardown_yield_fixture, fixturefunc, generator))
    else:
        value = fixturefunc(**kwargs)
    return value


def _fail_multi_yield(generator):
    """Report a yield fixture that yielded twice, mirroring upstream's
    fail_fixturefunc: the message carries the fixture's location and there is
    no traceback (pytrace=False)."""
    from _pytest.outcomes import fail

    msg = "fixture function has more than one 'yield'"
    code = getattr(generator, "gi_code", None)
    if code is None:
        fail(msg, pytrace=False)
    location = f"{code.co_filename}:{code.co_firstlineno}"
    fail(f"{msg}:\n\n{location}", pytrace=False)


def finalize_generator(generator):
    """Advance a yield fixture's generator at teardown; a second yield is an
    error reported like upstream (message + location, no traceback)."""
    try:
        next(generator)
    except StopIteration:
        return
    _fail_multi_yield(generator)


def _teardown_yield_fixture(fixturefunc, it):
    """Drain the rest of a yield fixture's generator at teardown; a second
    yield is an error, like upstream."""
    finalize_generator(it)


def fixture_lookup_error(argname, requesting_funcs, available):
    """Build a rich FixtureLookupError for an unknown fixture, mirroring
    upstream's FixtureLookupErrorRepr: the def line(s) of every fixture in the
    request chain (outermost first), "fixture 'X' not found", the sorted
    available-fixtures list, and the --fixtures help line. pytest._tb.
    format_exception renders the attached metadata."""
    from pytest._fixtures import FixtureLookupError

    # Accept a single callable or a chain of them (the resolution stack).
    if not isinstance(requesting_funcs, (list, tuple)):
        requesting_funcs = [requesting_funcs]
    deflines = []
    for func in requesting_funcs:
        try:
            lines, _ = inspect.getsourcelines(func)
        except (OSError, TypeError, IndexError):
            # Upstream's formatrepr prints "file X, line Y: source code not
            # available" when getsourcelines fails; a synthetic def line
            # would swallow that message (#553).
            deflines.append("  source code not available")
            continue
        for raw in lines:
            stripped = raw.rstrip()
            deflines.append("  " + stripped)
            if "):" in stripped or stripped.endswith(":"):
                break
    errstring = (
        f"fixture '{argname}' not found\n"
        f"available fixtures: {', '.join(available)}\n"
        "use 'pytest --fixtures [testpath]' for help on them."
    )
    err = FixtureLookupError(errstring)
    err._fixture_lookup_deflines = deflines
    err._fixture_lookup_errstring = errstring
    return err


def fail_subrequest_no_param(nodeid, fixturefunc, argname, rootpath):
    """Report request.getfixturevalue() of a parametrized fixture that has no
    parameter bound for this test, mirroring upstream's message: the test
    nodeid, the fixture's definition location, and the call site. Run from the
    engine's resolver; the engine's Rust frames are invisible to Python, so the
    Python frame directly below this helper is the requesting code."""
    from _pytest.outcomes import fail

    def _loc(filename, lineno):
        p = Path(filename)
        try:
            p = p.relative_to(rootpath)
        except ValueError:
            pass
        return f"{p}:{lineno}"

    real = getattr(fixturefunc, "__wrapped__", fixturefunc)
    code = real.__code__
    # getlocation() reports co_firstlineno + 1 (the def line for a singly
    # decorated fixture).
    fixture_loc = _loc(code.co_filename, code.co_firstlineno + 1)
    frame = sys._getframe(1)
    here_loc = _loc(frame.f_code.co_filename, frame.f_lineno)

    fail(
        "The requested fixture has no parameter defined for test:\n"
        f"    {nodeid}\n\n"
        f"Requested fixture '{argname}' defined in:\n"
        f"{fixture_loc}\n\n"
        "Requested here:\n"
        f"{here_loc}",
        pytrace=False,
    )


class FixtureFunctionDefinition:
    """pytest 8.4+ wraps @pytest.fixture functions in this. pytest-rs marks
    fixtures with recorded metadata instead, so nothing is ever an instance —
    but it must be a real class: hypothesis isinstance()s against it at
    @given application time (an _Unsupported stub raises TypeError there)."""


from _pytest._stub import __getattr__  # noqa: E402, F401
