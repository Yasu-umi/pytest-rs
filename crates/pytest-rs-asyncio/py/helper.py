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
    return loop.run_until_complete(coro)


def async_gen_first(loop, agen):
    return loop.run_until_complete(agen.__anext__())


def async_gen_finalizer(loop, agen):
    def _finalize():
        try:
            loop.run_until_complete(agen.__anext__())
        except StopAsyncIteration:
            pass
        else:
            raise RuntimeError("async fixture generator yielded more than once")

    return _finalize
