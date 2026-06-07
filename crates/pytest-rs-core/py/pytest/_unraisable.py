"""sys.unraisablehook capture: upstream _pytest/unraisableexception.py port.

The engine installs the hook at configure, drains collected unraisables
after each phase (setup / a passed call / teardown — upstream's trylast
hookimpls) and once more at session end after forcing gc (refcycles with
broken __del__, issue #10404).
"""

import collections
import functools
import gc
import sys
import traceback
import warnings
from typing import NamedTuple

import pytest

#: Constant determined experimentally by the Trio project.
_GC_COLLECT_ITERATIONS = 5

_deque = None
_prev_hook = None


def tracemalloc_message(source):
    """Upstream _pytest/tracemalloc.py."""
    if source is None:
        return ""

    try:
        import tracemalloc
    except ImportError:
        return ""

    tb = tracemalloc.get_object_traceback(source)
    if tb is not None:
        formatted_tb = "\n".join(tb.format())
        # Use a leading new line to better separate the (large) output
        # from the traceback to the previous warning text.
        return f"\nObject allocated at:\n{formatted_tb}"
    # No need for a leading new line.
    url = "https://docs.pytest.org/en/stable/how-to/capture-warnings.html#resource-warnings"
    return (
        "Enable tracemalloc to get traceback where the object was allocated.\n"
        f"See {url} for more info."
    )


class UnraisableMeta(NamedTuple):
    msg: str
    cause_msg: str
    exc_value: BaseException | None


def unraisable_hook(unraisable, /, *, append):
    try:
        # we need to compute these strings here as they might change after
        # the unraisablehook finishes and before the metadata object is
        # collected by a pytest hook
        err_msg = "Exception ignored in" if unraisable.err_msg is None else unraisable.err_msg
        summary = f"{err_msg}: {unraisable.object!r}"
        traceback_message = "\n\n" + "".join(
            traceback.format_exception(
                unraisable.exc_type,
                unraisable.exc_value,
                unraisable.exc_traceback,
            )
        )
        tracemalloc_tb = "\n" + tracemalloc_message(unraisable.object)
        msg = summary + traceback_message + tracemalloc_tb
        cause_msg = summary + tracemalloc_tb

        append(
            UnraisableMeta(
                msg=msg,
                cause_msg=cause_msg,
                exc_value=unraisable.exc_value,
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
    _prev_hook = sys.unraisablehook
    sys.unraisablehook = functools.partial(unraisable_hook, append=_deque.append)


def collect_unraisable():
    """Warn (or raise, under an error filter) for every collected
    unraisable exception — upstream collect_unraisable."""
    if _deque is None:
        return
    pop_unraisable = _deque.pop
    errors = []
    meta = None
    hook_error = None
    try:
        while True:
            try:
                meta = pop_unraisable()
            except IndexError:
                break

            if isinstance(meta, BaseException):
                hook_error = RuntimeError("Failed to process unraisable exception")
                hook_error.__cause__ = meta
                errors.append(hook_error)
                continue

            msg = meta.msg
            try:
                warnings.warn(pytest.PytestUnraisableExceptionWarning(msg))
            except pytest.PytestUnraisableExceptionWarning as e:
                # This except happens when the warning is treated as an error
                # (e.g. `-Werror`).
                if meta.exc_value is not None:
                    # Exceptions have a better way to show the traceback, but
                    # warnings do not, so hide the traceback from the msg and
                    # set the cause so the traceback shows up in the right
                    # place.
                    e.args = (meta.cause_msg,)
                    e.__cause__ = meta.exc_value
                errors.append(e)

        if len(errors) == 1:
            raise errors[0]
        if errors:
            raise ExceptionGroup("multiple unraisable exception warnings", errors)
    finally:
        del errors, meta, hook_error


def session_cleanup():
    """Force gc (a single collection doesn't necessarily collect
    everything), drain leftovers, restore the hook — upstream's config
    cleanup."""
    global _deque, _prev_hook
    if _deque is None:
        return
    try:
        try:
            for _ in range(_GC_COLLECT_ITERATIONS):
                gc.collect()
            collect_unraisable()
        finally:
            sys.unraisablehook = _prev_hook
    finally:
        _deque = None
        _prev_hook = None
