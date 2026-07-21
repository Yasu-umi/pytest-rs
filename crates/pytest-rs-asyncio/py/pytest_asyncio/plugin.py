"""The `pytest_asyncio.plugin` module shim: the decorator/inspection API
plus the unused-port fixtures (upstream tests monkeypatch `_unused_port`
on this module, so the fixtures must resolve it through the module
global)."""

import contextlib
import socket
from typing import TYPE_CHECKING, Any, overload

import pytest

if TYPE_CHECKING:
    from collections.abc import AsyncIterator, Awaitable, Callable, Iterable

    from _pytest.config import Config
    from _pytest.scope import _ScopeName


#: Upstream (pytest_asyncio.plugin.FixtureFunction) claims the decorator
#: preserves the wrapped function's own signature -- ergonomic parity for
#: callers, even though at runtime the decorator actually returns whatever
#: pytest.fixture() itself returns (FixtureFunctionDefinition).
@overload
def fixture[**P, R: "Awaitable[Any] | AsyncIterator[Any]"](
    fixture_function: "Callable[P, R]",
    *,
    scope: "_ScopeName | Callable[[str, Config], _ScopeName]" = ...,
    loop_scope: "_ScopeName | None" = ...,
    params: "Iterable[object] | None" = ...,
    autouse: bool = ...,
    ids: "Iterable[str | float | int | bool | None] | Callable[[Any], object | None] | None" = ...,
    name: str | None = ...,
) -> "Callable[P, R]": ...


@overload
def fixture[**P, R: "Awaitable[Any] | AsyncIterator[Any]"](
    fixture_function: None = ...,
    *,
    scope: "_ScopeName | Callable[[str, Config], _ScopeName]" = ...,
    loop_scope: "_ScopeName | None" = ...,
    params: "Iterable[object] | None" = ...,
    autouse: bool = ...,
    ids: "Iterable[str | float | int | bool | None] | Callable[[Any], object | None] | None" = ...,
    name: str | None = ...,
) -> "Callable[[Callable[P, R]], Callable[P, R]]": ...


def fixture(fixture_function=None, *, loop_scope=None, **kwargs):
    """pytest_asyncio.fixture: records the same metadata as pytest.fixture
    plus the loop scope for the asyncio plugin."""
    marker = pytest.fixture(**kwargs)

    def apply(func):
        func._pytest_asyncio_fixture = True
        if loop_scope is not None:
            func._pytest_asyncio_loop_scope = loop_scope
        func = marker(func)
        func._pytest_asyncio_fixture = True
        return func

    if fixture_function is not None:
        return apply(fixture_function)
    return apply


def is_async_test(item):
    """True for items the asyncio plugin will run (marked async tests)."""
    return item.get_closest_marker("asyncio") is not None


def _unused_port(socket_type):
    """Find an unused localhost port from 1024-65535 and return it."""
    with contextlib.closing(socket.socket(type=socket_type)) as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


@pytest.fixture
def unused_tcp_port():
    return _unused_port(socket.SOCK_STREAM)


@pytest.fixture
def unused_udp_port():
    return _unused_port(socket.SOCK_DGRAM)


@pytest.fixture(scope="session")
def unused_tcp_port_factory():
    """A factory function, producing different unused TCP ports."""
    produced = set()

    def factory():
        """Return an unused port."""
        port = _unused_port(socket.SOCK_STREAM)

        while port in produced:
            port = _unused_port(socket.SOCK_STREAM)

        produced.add(port)

        return port

    return factory


@pytest.fixture(scope="session")
def unused_udp_port_factory():
    """A factory function, producing different unused UDP ports."""
    produced = set()

    def factory():
        """Return an unused port."""
        port = _unused_port(socket.SOCK_DGRAM)

        while port in produced:
            port = _unused_port(socket.SOCK_DGRAM)

        produced.add(port)

        return port

    return factory
