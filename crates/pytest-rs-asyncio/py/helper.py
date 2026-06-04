"""Event-loop helpers for pytest-rs-asyncio."""

import asyncio


def new_loop():
    loop = asyncio.new_event_loop()
    asyncio.set_event_loop(loop)
    return loop


def close_loop(loop):
    try:
        loop.run_until_complete(loop.shutdown_asyncgens())
    finally:
        loop.close()
        asyncio.set_event_loop(None)


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
