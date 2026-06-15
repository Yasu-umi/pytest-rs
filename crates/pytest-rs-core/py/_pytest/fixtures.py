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


def _teardown_yield_fixture(fixturefunc, it):
    """Drain the rest of a yield fixture's generator at teardown; a second
    yield is an error, like upstream."""
    try:
        next(it)
    except StopIteration:
        pass
    else:
        raise ValueError(f"fixture function has more than one 'yield': {fixturefunc!r}")


class FixtureFunctionDefinition:
    """pytest 8.4+ wraps @pytest.fixture functions in this. pytest-rs marks
    fixtures with recorded metadata instead, so nothing is ever an instance —
    but it must be a real class: hypothesis isinstance()s against it at
    @given application time (an _Unsupported stub raises TypeError there)."""


from _pytest._stub import __getattr__  # noqa: E402, F401
