"""unittest.TestCase integration: build zero-arg runners per test method."""

import sys
import unittest

from pytest._outcomes import Skipped


def is_testcase_class(obj):
    try:
        return isinstance(obj, type) and issubclass(obj, unittest.TestCase)
    except TypeError:
        return False


class _SubtestRecorder:
    """Result object backing TestCase.subTest: routes addSubTest/addSkip
    into the pytest._subtests accumulator (records are tagged "unittest"
    because unittest subtest failures do not fail the enclosing test)."""

    def __init__(self, case) -> None:
        self._case = case
        self.failfast = False

    @staticmethod
    def _subtest_desc(subtest) -> str:
        from unittest.case import _subtest_msg_sentinel  # type: ignore[attr-defined]

        from pytest._subtests import _description

        message = getattr(subtest, "_message", _subtest_msg_sentinel)
        msg = None if message is _subtest_msg_sentinel else str(message)
        params = dict(getattr(subtest, "params", {}) or {})
        return _description(msg, params)

    @staticmethod
    def _record(desc, outcome, exc=None, reason="", location=None) -> None:
        from pytest._subtests import _results

        _results.append(
            {
                "desc": desc,
                "duration": 0.0,
                "exc": exc,
                "reason": reason,
                "location": location,
                "outcome": outcome,
                "unittest": True,
            }
        )

    def addSubTest(self, test_case, subtest, exc_info) -> None:
        desc = self._subtest_desc(subtest)
        if exc_info is None:
            self._record(desc, "passed")
        else:
            self._record(desc, "failed", exc=exc_info[1])

    def addSkip(self, test, reason) -> None:
        # Only subtest-level skips arrive here (main-body SkipTest
        # propagates out of the direct method call instead).
        location = None
        tb = sys.exc_info()[2]
        if tb is not None:
            while tb.tb_next is not None:
                tb = tb.tb_next
            location = f"{tb.tb_frame.f_code.co_filename}:{tb.tb_lineno}"
        self._record(self._subtest_desc(test), "skipped", reason=str(reason), location=location)

    def addError(self, test, exc_info) -> None:  # pragma: no cover - safety net
        pass

    def addFailure(self, test, exc_info) -> None:  # pragma: no cover - safety net
        pass


def make_runner(cls, method_name):
    """A zero-arg callable running setUp/method/tearDown with SkipTest
    mapped onto pytest's Skipped. A unittest _Outcome backs self.subTest()."""

    def _skipped_at(msg, func):
        """Skipped exception located at the test's definition line, so the
        -rs fold shows "file.py:N: reason" (not this shim)."""
        exc = Skipped(msg=msg)
        code = getattr(func, "__code__", None)
        if code is not None:
            import os

            try:
                filename = os.path.relpath(code.co_filename)
            except ValueError:
                filename = code.co_filename
            exc._location = f"{filename}:{code.co_firstlineno}"
        return exc

    def run():
        __tracebackhide__ = True
        case = cls(method_name)
        method = getattr(case, method_name)
        # Class-level skip decorators.
        if getattr(cls, "__unittest_skip__", False):
            raise _skipped_at(getattr(cls, "__unittest_skip_why__", ""), method)
        if getattr(method, "__unittest_skip__", False):
            raise _skipped_at(getattr(method, "__unittest_skip_why__", ""), method)

        from unittest.case import _Outcome, _ShouldStop

        outcome = _Outcome(_SubtestRecorder(case))
        expecting_failure = getattr(method, "__unittest_expecting_failure__", False) or getattr(
            cls, "__unittest_expecting_failure__", False
        )
        outcome.expecting_failure = expecting_failure
        case._outcome = outcome
        try:
            case.setUp()
        except unittest.SkipTest as e:
            raise Skipped(msg=str(e)) from None
        try:
            try:
                method()
            except unittest.SkipTest as e:
                raise Skipped(msg=str(e)) from None
            except _ShouldStop:
                # subTest aborts the body once an expected failure is seen
                # (TestCase.run catches this in its outer part executor).
                pass
        finally:
            case.tearDown()
            case._outcome = None
        if expecting_failure and outcome.expectedFailure is not None:
            # A subtest raised under @expectedFailure: surface it as xfail.
            from pytest._outcomes import XFailed

            exc = outcome.expectedFailure[1]
            if isinstance(exc, XFailed):
                raise exc
            raise XFailed(str(exc) or "expected failure")

    return run


def class_setup(cls):
    setup = getattr(cls, "setUpClass", None)
    if setup is not None:
        try:
            setup()
        except unittest.SkipTest as e:
            raise Skipped(msg=str(e)) from None


def class_teardown(cls):
    teardown = getattr(cls, "tearDownClass", None)
    if teardown is not None:
        teardown()
