"""The `pytest_asyncio` module shim: upstream suites import this."""

import pytest

__version__ = "1.4.0"  # pytest-asyncio API version this shim tracks


def fixture(fixture_function=None, *, loop_scope=None, **kwargs):
    """pytest_asyncio.fixture: records the same metadata as pytest.fixture.

    loop_scope is accepted but per-fixture loop scoping is not implemented
    yet (the function-scoped loop is used).
    """
    if fixture_function is not None:
        return pytest.fixture(fixture_function)
    return pytest.fixture(**kwargs)


def is_async_test(item):  # pragma: no cover - compat surface
    return False
