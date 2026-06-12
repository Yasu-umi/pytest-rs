"""Event-loop helpers for pytest-rs-asyncio."""

import asyncio
import contextvars
import functools
import inspect
import warnings


def new_loop():
    loop = asyncio.new_event_loop()
    asyncio.set_event_loop(loop)
    return loop


def new_loop_with_factory(factory):
    loop = factory()
    asyncio.set_event_loop(loop)
    return loop


def new_loop_with_policy(policy):
    """The (overridable) event_loop_policy fixture drives loop creation;
    pytest-asyncio also installs it as the current policy for the loop's
    lifetime — close_loop restores the previous one. The policy-API
    deprecations (Python 3.14) are suppressed for these internal calls, as
    upstream does."""
    with warnings.catch_warnings():
        warnings.simplefilter("ignore", DeprecationWarning)
        prev = asyncio.get_event_loop_policy()
        asyncio.set_event_loop_policy(policy)
    loop = policy.new_event_loop()
    loop._pytest_rs_prev_policy = prev
    asyncio.set_event_loop(loop)
    return loop


def set_current_loop(loop):
    asyncio.set_event_loop(loop)


def close_loop(loop):
    try:
        shutdown = loop.shutdown_asyncgens()
        try:
            loop.run_until_complete(shutdown)
        except BaseException:
            # Suppress the never-awaited warning when the loop is unusable.
            shutdown.close()
            raise
    except Exception as exc:
        # Upstream surfaces loop-teardown failures as a warning, not an
        # error (matching asyncio.Runner semantics).
        warnings.warn(
            f"An exception occurred during teardown of an asyncio.Runner: {exc!r}",
            RuntimeWarning,
        )
    finally:
        loop.close()
        asyncio.set_event_loop(None)
        prev = getattr(loop, "_pytest_rs_prev_policy", None)
        if prev is not None:
            with warnings.catch_warnings():
                warnings.simplefilter("ignore", DeprecationWarning)
                asyncio.set_event_loop_policy(prev)


def run(loop, coro):
    """Drive a coroutine in the item's contextvars context.

    The task is created with the *same* Context object (not a copy) so
    contextvars set inside async fixtures/tests propagate back to sync
    fixtures and vice versa.
    """
    import pytest._ctx as _ctx

    ctx = _ctx.current()
    if ctx is None:
        return loop.run_until_complete(coro)
    task = loop.create_task(coro, context=ctx)
    return loop.run_until_complete(task)


def adopt_context():
    """Adopt a copy of the current item context for subsequent fixture/test
    calls (upstream: contextvars set in an async fixture propagate to the
    test and are undone at the fixture's teardown). Returns (new, prev)."""
    import pytest._ctx as _ctx

    prev = _ctx._current
    if prev is None:
        new = contextvars.copy_context()
    else:
        new = prev.run(contextvars.copy_context)
    _ctx._current = new
    return new, prev


def context_restoring_finalizer(inner, new, prev):
    """Run the fixture's own finalizer (if any) inside its adopted context,
    then restore the context that was current before its setup."""
    import pytest._ctx as _ctx

    def _finalize():
        _ctx._current = new
        try:
            if inner is not None:
                inner()
        finally:
            _ctx._current = prev

    return _finalize


def hypothesis_async_inner(func):
    """The async inner_test behind a hypothesis-decorated callable, if any
    (unwrapping a shim installed by a previous parametrized run)."""
    hypothesis = getattr(func, "hypothesis", None)
    if hypothesis is None:
        return None
    inner = hypothesis.inner_test
    inner = getattr(inner, "__wrapped__", inner)
    if not inspect.iscoroutinefunction(inner):
        return None
    return inner


def hypothesis_wrap(loop, inner):
    """Sync shim around an async hypothesis inner_test: each example runs
    to completion on the item's event loop."""

    @functools.wraps(inner)
    def wrapper(*args, **kwargs):
        return run(loop, inner(*args, **kwargs))

    return wrapper


def sync_gen_finalizer(loop, gen):
    """Finalizer for sync generator fixtures owned by pytest-asyncio: the
    fixture's loop is re-installed as current before resuming, so teardown
    code sees the same loop as setup."""

    def _finalize():
        asyncio.set_event_loop(loop)
        try:
            next(gen)
        except StopIteration:
            pass
        else:
            raise RuntimeError("fixture function has more than one 'yield'")

    return _finalize


def async_gen_first(loop, agen):
    return run(loop, agen.__anext__())


def async_gen_finalizer(loop, agen):
    def _finalize():
        try:
            run(loop, agen.__anext__())
        except StopAsyncIteration:
            pass
        else:
            raise RuntimeError("async fixture generator yielded more than once")

    return _finalize
