"""The `pytest_asyncio` module shim: upstream suites import this."""

import pytest

__version__ = "1.4.0"  # pytest-asyncio API version this shim tracks


def fixture(fixture_function=None, *, loop_scope=None, **kwargs):
    """pytest_asyncio.fixture: records the same metadata as pytest.fixture
    plus the loop scope for the asyncio plugin."""
    marker = pytest.fixture(**kwargs)

    def apply(func):
        func = marker(func)
        func._pytest_asyncio_fixture = True
        if loop_scope is not None:
            func._pytest_asyncio_loop_scope = loop_scope
        return func

    if fixture_function is not None:
        return apply(fixture_function)
    return apply


def is_async_test(item):  # pragma: no cover - compat surface
    return False
