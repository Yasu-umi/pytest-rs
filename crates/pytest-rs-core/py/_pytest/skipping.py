"""skip/skipif/xfail evaluation over Item nodes — upstream
_pytest/skipping.py port (the engine's per-item evaluation lives in
pytest._skipping; this is the test-facing item API)."""

import dataclasses
import os
import platform
import sys
import traceback
from collections.abc import Mapping

from pytest._outcomes import fail, skip, xfail


def evaluate_condition(item, mark, condition):
    """Evaluate a single skipif/xfail condition, returning (result, reason)."""
    # String condition.
    if isinstance(condition, str):
        globals_ = {
            "os": os,
            "sys": sys,
            "platform": platform,
            "config": item.config,
        }
        for dictionary in reversed(item.ihook.pytest_markeval_namespace(config=item.config)):
            if not isinstance(dictionary, Mapping):
                raise ValueError(
                    f"pytest_markeval_namespace() needs to return a dict, got {dictionary!r}"
                )
            globals_.update(dictionary)
        if hasattr(item, "obj"):
            globals_.update(item.obj.__globals__)
        try:
            filename = f"<{mark.name} condition>"
            condition_code = compile(condition, filename, "eval")
            result = eval(condition_code, globals_)
        except SyntaxError as exc:
            msglines = [
                f"Error evaluating {mark.name!r} condition",
                "    " + condition,
                "    " + " " * (exc.offset or 0) + "^",
                "SyntaxError: invalid syntax",
            ]
            fail("\n".join(msglines), pytrace=False)
        except Exception as exc:
            msglines = [
                f"Error evaluating {mark.name!r} condition",
                "    " + condition,
                *traceback.format_exception_only(type(exc), exc),
            ]
            fail("\n".join(msglines), pytrace=False)

    # Boolean condition.
    else:
        try:
            result = bool(condition)
        except Exception as exc:
            msglines = [
                f"Error evaluating {mark.name!r} condition as a boolean",
                *traceback.format_exception_only(type(exc), exc),
            ]
            fail("\n".join(msglines), pytrace=False)

    reason = mark.kwargs.get("reason", None)
    if reason is None:
        if isinstance(condition, str):
            reason = "condition: " + condition
        else:
            msg = (
                f"Error evaluating {mark.name!r}: "
                + "you need to specify reason=STRING when using booleans as conditions."
            )
            fail(msg, pytrace=False)

    return result, reason


@dataclasses.dataclass(frozen=True)
class Skip:
    """The result of evaluate_skip_marks()."""

    reason: str = "unconditional skip"


def evaluate_skip_marks(item):
    """Evaluate skip and skipif marks on item, returning Skip if triggered."""
    for mark in item.iter_markers(name="skipif"):
        if "condition" not in mark.kwargs:
            conditions = mark.args
        else:
            conditions = (mark.kwargs["condition"],)

        # Unconditional.
        if not conditions:
            reason = mark.kwargs.get("reason", "")
            return Skip(reason)

        # If any of the conditions are true.
        for condition in conditions:
            result, reason = evaluate_condition(item, mark, condition)
            if result:
                return Skip(reason)

    for mark in item.iter_markers(name="skip"):
        try:
            return Skip(**mark.kwargs) if mark.kwargs else Skip(*mark.args)
        except TypeError as e:
            raise TypeError(str(e) + " - maybe you meant pytest.mark.skipif?") from None

    return None


class Xfail:
    """The result of evaluate_xfail_marks()."""

    __slots__ = ("raises", "reason", "run", "strict")

    def __init__(self, reason, run, strict, raises):
        self.reason = reason
        self.run = run
        self.strict = strict
        self.raises = raises


def _strict_default(config):
    for ini in ("strict_xfail", "xfail_strict"):
        try:
            value = config.getini(ini)
        except Exception:
            value = None
        if value is not None:
            return bool(value)
    return False


def evaluate_xfail_marks(item):
    """Evaluate xfail marks on item, returning Xfail if triggered."""
    for mark in item.iter_markers(name="xfail"):
        run = mark.kwargs.get("run", True)
        strict = mark.kwargs.get("strict")
        if strict is None:
            strict = _strict_default(item.config)
        raises = mark.kwargs.get("raises", None)
        if "condition" not in mark.kwargs:
            conditions = mark.args
        else:
            conditions = (mark.kwargs["condition"],)

        # Unconditional.
        if not conditions:
            reason = mark.kwargs.get("reason", "")
            return Xfail(reason, run, strict, raises)

        # If any of the conditions are true.
        for condition in conditions:
            result, reason = evaluate_condition(item, mark, condition)
            if result:
                return Xfail(reason, run, strict, raises)

    return None


def pytest_runtest_setup(item):
    skipped = evaluate_skip_marks(item)
    if skipped:
        raise skip.Exception(skipped.reason)

    xfailed = evaluate_xfail_marks(item)
    if xfailed and not xfailed.run:
        xfail("[NOTRUN] " + xfailed.reason)


from _pytest._stub import __getattr__  # noqa: E402, F401
