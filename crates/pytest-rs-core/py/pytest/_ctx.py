"""Per-item contextvars context.

pytest-asyncio runs fixtures and the test in one copied context so that
contextvars set in (sync or async) fixtures propagate into the test.
The runner begins/ends the context around each item; all fixture and test
calls route through call().
"""

import contextvars

_current: contextvars.Context | None = None


def begin_item():
    global _current
    _current = contextvars.copy_context()


def end_item():
    global _current
    _current = None


def current():
    return _current


def call(func, /, *args, **kwargs):
    __tracebackhide__ = True
    if _current is None:
        return func(*args, **kwargs)
    return _current.run(func, *args, **kwargs)
