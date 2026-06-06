"""anyio pytest plugin runtime, ported from anyio.pytest_plugin.

The engine drives the hooks natively; the installed anyio library provides
the backends (TestRunner) and its plugin module's fixtures (anyio_backend,
anyio_backend_name, free_tcp_port, ...) arrive via entry-point autoload.
anyio is imported lazily so the plugin stays inert when it is not installed.
"""

from contextlib import ExitStack, contextmanager

_current_runner = None
_runner_stack = None
_runner_leases = 0


def extract_backend_and_options(backend):
    if isinstance(backend, str):
        return backend, {}
    elif isinstance(backend, tuple) and len(backend) == 2:
        if isinstance(backend[0], str) and isinstance(backend[1], dict):
            return backend

    raise TypeError("anyio_backend must be either a string or tuple of (string, dict)")


@contextmanager
def get_runner(backend_name, backend_options):
    """One TestRunner shared by every nested lease: an async generator
    fixture holding its lease across setup..teardown keeps the runner (and
    its event loop) open for the tests inside its scope."""
    global _current_runner, _runner_leases, _runner_stack
    if _current_runner is None:
        from anyio._core._eventloop import get_async_backend

        asynclib = get_async_backend(backend_name)
        _runner_stack = ExitStack()
        # Cache the async library name while we own the loop. anyio >= 4.12
        # tracks it itself; older versions go through the sniffio cvar.
        try:
            from anyio._core._eventloop import (
                current_async_library,
                reset_current_async_library,
                set_current_async_library,
            )
        except ImportError:
            import sniffio

            if sniffio.current_async_library_cvar.get(None) is None:
                token = sniffio.current_async_library_cvar.set(backend_name)
                _runner_stack.callback(sniffio.current_async_library_cvar.reset, token)
        else:
            if current_async_library() is None:
                token = set_current_async_library(backend_name)
                _runner_stack.callback(reset_current_async_library, token)

        backend_options = backend_options or {}
        _current_runner = _runner_stack.enter_context(asynclib.create_test_runner(backend_options))

    _runner_leases += 1
    try:
        yield _current_runner
    finally:
        _runner_leases -= 1
        if not _runner_leases:
            _runner_stack.close()
            _runner_stack = _current_runner = None


_coroutine_running = False


@contextmanager
def _exclusive_run():
    """Reject re-entrant scheduling (request.getfixturevalue() on an async
    fixture from inside a running async test): asyncio would raise "event
    loop is already running", trio would deadlock — upstream raises this
    consistent error instead (anyio#1148)."""
    global _coroutine_running
    if _coroutine_running:
        raise RuntimeError(
            "Cannot schedule a coroutine in the test runner while another is "
            "already running; likely caused by request.getfixturevalue() on an "
            "async fixture."
        )
    _coroutine_running = True
    try:
        yield
    finally:
        _coroutine_running = False


def _iterate_exceptions(exc):
    if isinstance(exc, BaseExceptionGroup):
        for sub in exc.exceptions:
            yield from _iterate_exceptions(sub)
    else:
        yield exc


def run_test(func, backend, kwargs):
    from pytest._outcomes import Exit

    backend_name, backend_options = extract_backend_and_options(backend)
    with get_runner(backend_name, backend_options) as runner, _exclusive_run():
        try:
            runner.run_test(func, kwargs)
        except BaseExceptionGroup as excgrp:
            # Session-fatal outcomes must surface as themselves, not as a
            # group wrapper (upstream parity).
            for exc in _iterate_exceptions(excgrp):
                if isinstance(exc, (Exit, KeyboardInterrupt, SystemExit)):
                    raise exc from excgrp

            raise


def hypothesis_async_inner(func):
    """The hypothesis inner_test if it is (or wraps) a coroutine function,
    else None. A previously installed backend wrapper is unwrapped so each
    backend-parametrized item rewraps the original."""
    from inspect import iscoroutinefunction

    hypothesis = getattr(func, "hypothesis", None)
    inner = getattr(hypothesis, "inner_test", None) if hypothesis is not None else None
    if inner is None:
        return None
    inner = getattr(inner, "__anyio_original_inner__", inner)
    return inner if iscoroutinefunction(inner) else None


def hypothesis_wrap(inner, backend):
    """A sync inner_test driving each hypothesis example through the
    backend's runner (upstream's run_with_hypothesis)."""
    from functools import wraps

    @wraps(inner)
    def run_with_hypothesis(**kwargs):
        run_test(inner, backend, kwargs)

    run_with_hypothesis.__anyio_original_inner__ = inner
    return run_with_hypothesis


def bound(func, instance):
    """Bind a Test*-class fixture function to the test instance."""
    if instance is not None:
        return func.__get__(instance)
    return func


def run_fixture(func, instance, backend, kwargs):
    backend_name, backend_options = extract_backend_and_options(backend)
    with get_runner(backend_name, backend_options) as runner, _exclusive_run():
        return runner.run_fixture(bound(func, instance), kwargs)


class AsyncGenFixture:
    """An async generator fixture's runner lease, held open from setup until
    the finalizer runs (anyio's `yield from runner.run_asyncgen_fixture()`,
    split into the engine's value + finalizer shape)."""

    def __init__(self, func, instance, backend, kwargs):
        backend_name, backend_options = extract_backend_and_options(backend)
        self._stack = ExitStack()
        runner = self._stack.enter_context(get_runner(backend_name, backend_options))
        self._gen = iter(runner.run_asyncgen_fixture(bound(func, instance), kwargs))

    def setup(self):
        try:
            with _exclusive_run():
                return next(self._gen)
        except BaseException:
            self._stack.close()
            raise

    def finalize(self):
        try:
            # The runner resumes the generator and raises if it yields again.
            with _exclusive_run():
                next(self._gen, None)
        finally:
            self._stack.close()
