"""threading.excepthook capture: upstream _pytest/threadexception.py port.

The engine installs the hook at configure, drains collected thread
exceptions after each phase (setup / a passed call / teardown) and once more
at session end. Mirrors pytest._unraisable's structure.
"""

import collections
import functools
import threading
import traceback
import warnings
from typing import NamedTuple

import pytest
from pytest._unraisable import tracemalloc_message

_deque = None
_prev_hook = None


class ThreadExceptionMeta(NamedTuple):
    msg: str
    cause_msg: str
    exc_value: BaseException | None


def thread_exception_hook(args, /, *, append):
    try:
        # Compute these strings here as they might change after the excepthook
        # finishes and before the metadata object is collected by a pytest hook.
        thread_name = "<unknown>" if args.thread is None else args.thread.name
        summary = f"Exception in thread {thread_name}"
        traceback_message = "\n\n" + "".join(
            traceback.format_exception(
                args.exc_type,
                args.exc_value,
                args.exc_traceback,
            )
        )
        tracemalloc_tb = "\n" + tracemalloc_message(args.thread)
        msg = summary + traceback_message + tracemalloc_tb
        cause_msg = summary + tracemalloc_tb

        append(
            ThreadExceptionMeta(
                msg=msg,
                cause_msg=cause_msg,
                exc_value=args.exc_value,
            )
        )
    except BaseException as e:
        append(e)
        # Raising this will cause the exception to be logged twice, which is
        # fine - this should never happen anyway.
        raise


def configure():
    global _deque, _prev_hook
    _deque = collections.deque()
    _prev_hook = threading.excepthook
    threading.excepthook = functools.partial(thread_exception_hook, append=_deque.append)


def collect_thread_exception():
    """Warn (or raise, under an error filter) for every collected unhandled
    thread exception — upstream collect_thread_exception."""
    if _deque is None:
        return
    pop_thread_exception = _deque.pop
    errors = []
    meta = None
    hook_error = None
    try:
        while True:
            try:
                meta = pop_thread_exception()
            except IndexError:
                break

            if isinstance(meta, BaseException):
                hook_error = RuntimeError("Failed to process thread exception")
                hook_error.__cause__ = meta
                errors.append(hook_error)
                continue

            msg = meta.msg
            try:
                warnings.warn(pytest.PytestUnhandledThreadExceptionWarning(msg))
            except pytest.PytestUnhandledThreadExceptionWarning as e:
                # This except happens when the warning is treated as an error
                # (e.g. `-Werror`).
                if meta.exc_value is not None:
                    e.args = (meta.cause_msg,)
                    e.__cause__ = meta.exc_value
                errors.append(e)

        if len(errors) == 1:
            raise errors[0]
        if errors:
            raise ExceptionGroup("multiple thread exception warnings", errors)
    finally:
        del errors, meta, hook_error


def session_cleanup():
    """Drain leftovers and restore the hook (upstream's config cleanup; no gc,
    unlike unraisable)."""
    global _deque, _prev_hook
    if _deque is None:
        return
    try:
        try:
            collect_thread_exception()
        finally:
            threading.excepthook = _prev_hook
    finally:
        _deque = None
        _prev_hook = None
