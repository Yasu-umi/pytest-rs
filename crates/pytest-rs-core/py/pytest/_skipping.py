"""skip/skipif/xfail mark evaluation (port of pytest's _pytest/skipping.py).

The engine passes the item's marks as (name, mark) pairs in closest-first
order plus the test module name; condition strings eval against
os/sys/platform/config plus the module's globals, exactly like pytest.
"""

import os
import platform
import sys
import traceback
from collections.abc import Mapping

from pytest._outcomes import fail


class Skip:
    # The signature carries pytest's TypeError messages for bad
    # @pytest.mark.skip usage ("Skip.__init__() got multiple values ...").
    def __init__(self, reason="unconditional skip"):
        self.reason = reason


def _condition_globals(module_name, config, namespaces):
    globals_ = {"os": os, "sys": sys, "platform": platform, "config": config}
    # conftest pytest_markeval_namespace contributions (later hooks first).
    for dictionary in reversed(namespaces or []):
        if not isinstance(dictionary, Mapping):
            raise ValueError(
                f"pytest_markeval_namespace() needs to return a dict, got {dictionary!r}"
            )
        globals_.update(dictionary)
    module = sys.modules.get(module_name)
    if module is not None:
        globals_.update(vars(module))
    return globals_


def evaluate_condition(mark_name, kwargs, condition, module_name, config, namespaces=None):
    """One skipif/xfail condition -> (triggered, reason). pytest.fail
    (no traceback) on evaluation errors and reason-less boolean conditions."""
    if isinstance(condition, str):
        globals_ = _condition_globals(module_name, config, namespaces)
        try:
            condition_code = compile(condition, f"<{mark_name} condition>", "eval")
            result = eval(condition_code, globals_)
        except SyntaxError as exc:
            msglines = [
                f"Error evaluating {mark_name!r} condition",
                "    " + condition,
                "    " + " " * (exc.offset or 0) + "^",
                "SyntaxError: invalid syntax",
            ]
            fail("\n".join(msglines), pytrace=False)
        except Exception as exc:
            msglines = [
                f"Error evaluating {mark_name!r} condition",
                "    " + condition,
                *traceback.format_exception_only(type(exc), exc),
            ]
            fail("\n".join(msglines), pytrace=False)
    else:
        try:
            result = bool(condition)
        except Exception as exc:
            msglines = [
                f"Error evaluating {mark_name!r} condition as a boolean",
                *traceback.format_exception_only(type(exc), exc),
            ]
            fail("\n".join(msglines), pytrace=False)

    reason = kwargs.get("reason", None)
    if reason is None:
        if isinstance(condition, str):
            reason = "condition: " + condition
        else:
            fail(
                f"Error evaluating {mark_name!r}: "
                "you need to specify reason=STRING when using booleans as conditions.",
                pytrace=False,
            )

    return bool(result), reason


def _mark_conditions(mark):
    if "condition" not in mark.kwargs:
        return mark.args
    return (mark.kwargs["condition"],)


def _is_module_mark(mark, module_name):
    """Did this mark come from the module-level pytestmark variable?
    (Such skips fold without a line number in the -rs summary.)"""
    module = sys.modules.get(module_name)
    pytestmark = getattr(module, "pytestmark", None)
    if pytestmark is None:
        return False
    entries = pytestmark if isinstance(pytestmark, (list, tuple)) else [pytestmark]
    return any(entry is mark or getattr(entry, "mark", None) is mark for entry in entries)


def evaluate_skip_marks(marks, module_name, config, namespaces=None):
    """(reason, from_pytestmark) when the item should skip, else None.
    marks: [(name, mark), ...] in closest-first order."""
    for name, mark in marks:
        if name != "skipif":
            continue
        conditions = _mark_conditions(mark)
        if not conditions:
            # Unconditional.
            return (mark.kwargs.get("reason", ""), _is_module_mark(mark, module_name))
        # If any of the conditions are true.
        for condition in conditions:
            result, reason = evaluate_condition(
                name, mark.kwargs, condition, module_name, config, namespaces
            )
            if result:
                return (reason, _is_module_mark(mark, module_name))

    for name, mark in marks:
        if name != "skip":
            continue
        try:
            reason = Skip(*mark.args, **mark.kwargs).reason
        except TypeError as e:
            raise TypeError(str(e) + " - maybe you meant pytest.mark.skipif?") from None
        return (reason, _is_module_mark(mark, module_name))

    return None


def evaluate_xfail_marks(marks, module_name, config, strict_default, namespaces=None):
    """(reason, run, strict, raises) for the first triggered xfail mark,
    or None."""
    for name, mark in marks:
        if name != "xfail":
            continue
        run = mark.kwargs.get("run", True)
        strict = mark.kwargs.get("strict")
        if strict is None:
            strict = strict_default
        raises = mark.kwargs.get("raises", None)
        conditions = _mark_conditions(mark)
        if not conditions:
            # Unconditional.
            return (str(mark.kwargs.get("reason", "")), bool(run), bool(strict), raises)
        # If any of the conditions are true.
        for condition in conditions:
            result, reason = evaluate_condition(
                name, mark.kwargs, condition, module_name, config, namespaces
            )
            if result:
                return (str(reason), bool(run), bool(strict), raises)

    return None
