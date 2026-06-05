"""subtests fixture: per-subtest outcomes collected by the Rust runner.

The context manager records each subtest block's outcome into a module
accumulator; after the call phase the runner pops the records and turns
them into individual reports (upstream pytest's builtin subtests plugin).
"""

import time
from typing import Any, Dict, List, Optional

from pytest._fixtures import fixture

# Per-item accumulator: the runner pops it after each call phase, and the
# fixture clears it on creation so an aborted item can't leak records.
_results: List[Dict[str, Any]] = []


def _saferepr(obj: Any) -> str:
    try:
        return repr(obj)
    except Exception as exc:
        return f"<[{exc!r} raised in repr()] {type(obj).__name__} object>"


class SubtestContext:
    """The values passed to Subtests.test() included in the test report."""

    def __init__(self, *, msg: Optional[str] = None, kwargs: Optional[dict] = None):
        self.msg = msg
        self.kwargs = dict(kwargs or {})

    def __eq__(self, other: Any) -> bool:
        return (
            isinstance(other, SubtestContext)
            and self.msg == other.msg
            and self.kwargs == other.kwargs
        )

    def __repr__(self) -> str:
        return f"SubtestContext(msg={self.msg!r}, kwargs={self.kwargs!r})"

    def _to_json(self) -> Dict[str, Any]:
        return {"msg": self.msg, "kwargs": dict(self.kwargs)}

    @classmethod
    def _from_json(cls, d: Dict[str, Any]) -> "SubtestContext":
        return cls(msg=d["msg"], kwargs=d["kwargs"])


def _description(msg: Optional[str], kwargs: dict) -> str:
    parts = []
    if msg is not None:
        parts.append(f"[{msg}]")
    if kwargs:
        params_desc = ", ".join(f"{k}={_saferepr(v)}" for k, v in kwargs.items())
        parts.append(f"({params_desc})")
    return " ".join(parts) or "(<subtest>)"


class SubtestReport:
    """Serializable subtest report (xdist wire format compatibility)."""

    def __init__(self) -> None:
        self.context = SubtestContext()

    @property
    def head_line(self) -> str:
        domain = getattr(self, "location", ("", "", ""))[2]
        return f"{domain} {self._sub_test_description()}"

    def _sub_test_description(self) -> str:
        return _description(self.context.msg, self.context.kwargs)

    def _to_json(self) -> Dict[str, Any]:
        data = dict(getattr(self, "__dict__", {}))
        data.pop("context", None)
        data["_report_type"] = "SubTestReport"
        data["_subtest.context"] = self.context._to_json()
        return data

    @classmethod
    def _from_json(cls, reportdict: Dict[str, Any]) -> "SubtestReport":
        report = cls()
        for key, value in reportdict.items():
            if key in ("_report_type", "_subtest.context"):
                continue
            setattr(report, key, value)
        report.context = SubtestContext._from_json(reportdict["_subtest.context"])
        return report


class _SubTestContextManager:
    """Records the subtest block's outcome; swallows ordinary failures so
    the enclosing test continues (upstream _SubTestContextManager)."""

    def __init__(self, msg: Optional[str], kwargs: dict) -> None:
        self.msg = msg
        self.kwargs = kwargs

    def __enter__(self) -> None:
        __tracebackhide__ = True
        self._start = time.perf_counter()

    def __exit__(self, exc_type, exc_val, exc_tb) -> bool:
        __tracebackhide__ = True
        duration = time.perf_counter() - self._start
        from pytest._outcomes import Exit, Skipped, XFailed

        record: Dict[str, Any] = {
            "desc": _description(self.msg, self.kwargs),
            "duration": duration,
            "exc": None,
            "reason": "",
            "location": None,
        }
        if exc_val is None:
            record["outcome"] = "passed"
        elif isinstance(exc_val, Skipped):
            record["outcome"] = "skipped"
            record["reason"] = str(exc_val)
            record["location"] = self._raise_location(exc_tb)
        elif isinstance(exc_val, XFailed):
            record["outcome"] = "xfailed"
            record["reason"] = str(exc_val)
            record["location"] = self._raise_location(exc_tb)
        else:
            record["outcome"] = "failed"
            record["exc"] = exc_val
        _results.append(record)

        if exc_val is not None and isinstance(
            exc_val, (KeyboardInterrupt, SystemExit, Exit)
        ):
            return False
        return True

    @staticmethod
    def _raise_location(tb) -> Optional[str]:
        """'file.py:line' of the innermost frame (the skip call site)."""
        if tb is None:
            return None
        while tb.tb_next is not None:
            tb = tb.tb_next
        return f"{tb.tb_frame.f_code.co_filename}:{tb.tb_lineno}"


class Subtests:
    """Declares subtests inside test functions via the test() method."""

    def test(self, msg: Optional[str] = None, **kwargs: Any) -> _SubTestContextManager:
        return _SubTestContextManager(msg, kwargs)


@fixture
def subtests():
    """Provides subtests functionality."""
    _results.clear()
    return Subtests()


def pop_results() -> List[Dict[str, Any]]:
    """Drain the accumulator (called by the runner after each call phase)."""
    out = list(_results)
    _results.clear()
    return out
