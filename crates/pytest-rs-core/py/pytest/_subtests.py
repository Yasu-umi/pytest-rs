"""subtests fixture: per-subtest outcomes collected by the Rust runner.

The context manager records each subtest block's outcome into a module
accumulator; after the call phase the runner pops the records and turns
them into individual reports (upstream pytest's builtin subtests plugin).
"""

import time
from typing import Any

from pytest._fixtures import fixture

# Per-item accumulator: the runner pops it after each call phase, and the
# fixture clears it on creation so an aborted item can't leak records.
_results: list[dict[str, Any]] = []

# --maxfail budget remaining for the session (None = unlimited). When a
# failed subtest exhausts it, the exception propagates so the test body
# stops, matching upstream's session.shouldfail check in __exit__.
_fail_budget: int | None = None

# When True, __exit__ writes the progress char to stdout immediately
# (needed with -s so chars interleave with test output).
_inline_chars: bool = False
_inline_count: int = 0

_PROGRESS_CHARS = {
    "passed": ",",
    "failed": "u",
    "skipped": "-",
    "xfailed": "y",
}


def set_fail_budget(budget: int | None) -> None:
    """Called by the runner before each item; also drops stale records."""
    global _fail_budget, _inline_count
    _fail_budget = budget
    _inline_count = 0
    _results.clear()


def set_inline_chars(enabled: bool) -> None:
    global _inline_chars
    _inline_chars = enabled


def pop_inline_count() -> int:
    global _inline_count
    n = _inline_count
    _inline_count = 0
    return n


def _saferepr(obj: Any) -> str:
    try:
        return repr(obj)
    except Exception as exc:
        return f"<[{exc!r} raised in repr()] {type(obj).__name__} object>"


class SubtestContext:
    """The values passed to Subtests.test() included in the test report."""

    def __init__(self, *, msg: str | None = None, kwargs: dict | None = None):
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

    def _to_json(self) -> dict[str, Any]:
        return {"msg": self.msg, "kwargs": dict(self.kwargs)}

    @classmethod
    def _from_json(cls, d: dict[str, Any]) -> "SubtestContext":
        return cls(msg=d["msg"], kwargs=d["kwargs"])


def _description(msg: str | None, kwargs: dict) -> str:
    parts = []
    if msg is not None:
        parts.append(f"[{msg}]")
    if kwargs:
        params_desc = ", ".join(f"{k}={_saferepr(v)}" for k, v in kwargs.items())
        parts.append(f"({params_desc})")
    return " ".join(parts) or "(<subtest>)"


class SubtestReport:
    """Serializable subtest report (xdist wire format compatibility)."""

    __test__ = False

    def __init__(
        self,
        nodeid: str | None = None,
        location: tuple | None = None,
        keywords: dict | None = None,
        outcome: str | None = None,
        when: str | None = None,
        longrepr: Any = None,
        sections: tuple = (),
        duration: float = 0.0,
        context: SubtestContext | None = None,
        **kw: Any,
    ) -> None:
        self.nodeid = nodeid
        self.location = location
        self.keywords = keywords or {}
        self.outcome = outcome
        self.when = when
        self.longrepr = longrepr
        self.sections = list(sections)
        self.duration = duration
        self.context = context or SubtestContext()
        for key, value in kw.items():
            setattr(self, key, value)

    @property
    def passed(self) -> bool:
        return self.outcome == "passed"

    @property
    def failed(self) -> bool:
        return self.outcome == "failed"

    @property
    def skipped(self) -> bool:
        return self.outcome == "skipped"

    @property
    def count_towards_summary(self) -> bool:
        return True

    @property
    def head_line(self) -> str:
        domain = getattr(self, "location", ("", "", ""))[2]
        return f"{domain} {self._sub_test_description()}"

    def _sub_test_description(self) -> str:
        return _description(self.context.msg, self.context.kwargs)

    def _to_json(self) -> dict[str, Any]:
        data = dict(getattr(self, "__dict__", {}))
        data.pop("context", None)
        data["_report_type"] = "SubTestReport"
        data["_subtest.context"] = self.context._to_json()
        return data

    @classmethod
    def _from_json(cls, reportdict: dict[str, Any]) -> "SubtestReport":
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

    def __init__(self, msg: str | None, kwargs: dict) -> None:
        self.msg = msg
        self.kwargs = kwargs

    def __enter__(self) -> None:
        __tracebackhide__ = True
        self._start = time.perf_counter()
        from pytest._capture import state as _capture_state
        from pytest._logging import state as _log_state

        _capture_state.subtest_enter()
        _log_state.subtest_enter()

    def __exit__(self, exc_type, exc_val, exc_tb) -> bool:
        __tracebackhide__ = True
        duration = time.perf_counter() - self._start
        from pytest._capture import state as _capture_state
        from pytest._logging import state as _log_state
        from pytest._outcomes import Exit, Skipped, XFailed

        sections = _capture_state.subtest_exit()
        sections.extend(_log_state.subtest_exit())

        record: dict[str, Any] = {
            "desc": _description(self.msg, self.kwargs),
            "duration": duration,
            "exc": None,
            "reason": "",
            "location": None,
            "sections": sections,
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

        if _inline_chars:
            import sys

            c = _PROGRESS_CHARS.get(record["outcome"])
            if c is not None:
                global _inline_count
                sys.stdout.write(c)
                sys.stdout.flush()
                _inline_count += 1

        if exc_val is not None and isinstance(exc_val, (KeyboardInterrupt, SystemExit, Exit)):
            return False
        if record["outcome"] == "failed":
            global _fail_budget
            if _fail_budget is not None:
                _fail_budget -= 1
                if _fail_budget <= 0:
                    return False
            from pytest._debugging import maybe_interact

            maybe_interact(None, exc_val)
        return True

    @staticmethod
    def _raise_location(tb) -> str | None:
        """'file.py:line' of the innermost frame (the skip call site)."""
        if tb is None:
            return None
        while tb.tb_next is not None:
            tb = tb.tb_next
        return f"{tb.tb_frame.f_code.co_filename}:{tb.tb_lineno}"


class Subtests:
    """Declares subtests inside test functions via the test() method."""

    def test(self, msg: str | None = None, **kwargs: Any) -> _SubTestContextManager:
        return _SubTestContextManager(msg, kwargs)


_compat_cls: type | None = None


def _make_subtests() -> Subtests:
    """Return a Subtests instance that passes isinstance(x, SubTests) when
    the pytest-subtests plugin is installed."""
    global _compat_cls
    if _compat_cls is not None:
        return _compat_cls()
    try:
        from pytest_subtests import SubTests as PluginCls
    except ImportError:
        return Subtests()
    _compat_cls = type(
        "Subtests",
        (PluginCls,),
        {
            "__init__": lambda self: None,
            "test": Subtests.test,
        },
    )
    return _compat_cls()


@fixture
def subtests():
    """Provides subtests functionality."""
    _results.clear()
    return _make_subtests()


def pytest_report_to_serializable(report: Any) -> dict[str, Any] | None:
    if isinstance(report, SubtestReport):
        return report._to_json()
    return None


def pytest_report_from_serializable(data: dict[str, Any]) -> SubtestReport | None:
    if data.get("_report_type") == "SubTestReport":
        return SubtestReport._from_json(data)
    return None


def pop_results() -> list[dict[str, Any]]:
    """Drain the accumulator (called by the runner after each call phase)."""
    out = list(_results)
    _results.clear()
    return out
