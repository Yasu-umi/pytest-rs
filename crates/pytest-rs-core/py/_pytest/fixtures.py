import functools
import inspect

import pytest

# Set on the pytest module by the Rust engine at startup (a pyo3 class).
FixtureRequest = getattr(pytest, "FixtureRequest", object)


from pytest._fixtures import FixtureLookupError as FixtureLookupError  # noqa: E402


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

    def __repr__(self):
        return f"<FixtureRequest for {self._pyfuncitem!r}>"


def call_fixture_func(fixturefunc, request, kwargs):
    """Call a fixture-style function, honoring a single `yield` for teardown
    (pytest-bdd runs step functions through this so steps may yield). Mirrors
    _pytest.fixtures.call_fixture_func for the sync case."""
    if inspect.isgeneratorfunction(fixturefunc):
        generator = fixturefunc(**kwargs)
        try:
            value = next(generator)
        except StopIteration:
            raise ValueError(f"{request.fixturename} did not yield a value") from None
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


class FixtureFunctionDefinition:
    """pytest 8.4+ wraps @pytest.fixture functions in this. pytest-rs marks
    fixtures with recorded metadata instead, so nothing is ever an instance —
    but it must be a real class: hypothesis isinstance()s against it at
    @given application time (an _Unsupported stub raises TypeError there)."""


from _pytest._stub import __getattr__  # noqa: E402, F401
