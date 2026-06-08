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


def _process_teardown_exceptions(cls):
    """Raise errors collected by doClassCleanups (upstream
    process_teardown_exceptions)."""
    exc_infos = getattr(cls, "tearDown_exceptions", None)
    if not exc_infos:
        return
    exceptions = [exc for (_, exc, _) in exc_infos]
    # A single exception is raised directly for a more readable error.
    if len(exceptions) == 1:
        raise exceptions[0]
    raise ExceptionGroup("Unittest class cleanup errors", exceptions)


def make_class_fixture(cls):
    """Upstream _register_unittest_setup_class_fixture: a class-scoped
    autouse fixture invoking setUpClass/tearDownClass + doClassCleanups."""
    import pytest

    setup = getattr(cls, "setUpClass", None)
    teardown = getattr(cls, "tearDownClass", None)
    if setup is None and teardown is None:
        return None
    cleanup = getattr(cls, "doClassCleanups", lambda: None)

    @pytest.fixture(
        scope="class",
        autouse=True,
        name=f"_unittest_setUpClass_fixture_{cls.__qualname__}",
    )
    def unittest_setup_class_fixture():
        if setup is not None:
            try:
                setup()
            except unittest.SkipTest as e:
                raise Skipped(msg=str(e)) from None
            # unittest does not call the cleanup function for every
            # BaseException, so we follow this here (upstream).
            except Exception:
                cleanup()
                _process_teardown_exceptions(cls)
                raise
        yield
        try:
            if teardown is not None:
                teardown()
        finally:
            cleanup()
            _process_teardown_exceptions(cls)

    return unittest_setup_class_fixture


def make_setup_class_fixture(cls):
    """Upstream _register_setup_class_fixture: pytest-style
    setup_class/teardown_class on a TestCase class."""
    import pytest
    from pytest._xunit import call_optional

    setup = getattr(cls, "setup_class", None)
    teardown = getattr(cls, "teardown_class", None)
    if setup is None and teardown is None:
        return None

    @pytest.fixture(
        scope="class",
        autouse=True,
        name=f"_xunit_setup_class_fixture_{cls.__qualname__}",
    )
    def xunit_setup_class_fixture():
        if setup is not None:
            call_optional(getattr(setup, "__func__", setup), cls)
        yield
        if teardown is not None:
            call_optional(getattr(teardown, "__func__", teardown), cls)

    return xunit_setup_class_fixture


def make_setup_method_fixture(cls):
    """Upstream _register_unittest_setup_method_fixture: pytest-style
    setup_method/teardown_method on a TestCase class (bound per test to
    the same instance the runner uses)."""
    import pytest

    setup = getattr(cls, "setup_method", None)
    teardown = getattr(cls, "teardown_method", None)
    if setup is None and teardown is None:
        return None

    @pytest.fixture(
        scope="function",
        autouse=True,
        name=f"_unittest_setup_method_fixture_{cls.__qualname__}",
    )
    def unittest_setup_method_fixture(self, request):
        method = getattr(self, request.node.name.split("[")[0], None)
        if setup is not None:
            setup(self, method)
        yield
        if teardown is not None:
            teardown(self, method)

    return unittest_setup_method_fixture


def make_runner(cls, method_name):
    """A zero-arg callable running setUp/method/tearDown with SkipTest
    mapped onto pytest's Skipped. A unittest _Outcome backs self.subTest().

    The callable also exposes make_case(): the engine calls it before
    fixture setup so @pytest.fixture(autouse=True) METHODS defined on the
    TestCase bind to the same instance the test runs on (upstream's
    item.instance)."""

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

    pending = []

    def make_case():
        """Create (and queue) the next run's TestCase instance — the
        fixture-binding instance the engine passes to fixture methods."""
        case = cls(method_name)
        pending.append(case)
        return case

    def run():
        __tracebackhide__ = True
        case = pending.pop() if pending else cls(method_name)
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
        primary = None
        # Django's SimpleTestCase/TestCase wrap each test in _pre_setup
        # (transaction begin + fixture load) / _post_teardown (rollback);
        # upstream runs these via TestCase.__call__, which our manual
        # setUp/method/tearDown loop bypasses. Call them when present.
        pre_setup = getattr(case, "_pre_setup", None)
        post_teardown = getattr(case, "_post_teardown", None)
        try:
            if pre_setup is not None:
                pre_setup()
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
                except BaseException as exc:
                    if expecting_failure:
                        # @unittest.expectedFailure: any body exception is
                        # the expected failure (xfail).
                        from pytest._outcomes import XFailed

                        raise XFailed(str(exc) or "expected failure") from exc
                    raise
                else:
                    if expecting_failure:
                        from pytest._outcomes import Failed

                        failure = Failed(msg="Unexpected success")
                        # Upstream: bare message, no traceback.
                        failure.pytrace = False
                        raise failure
            finally:
                case.tearDown()
        except BaseException as exc:
            primary = exc
        finally:
            if post_teardown is not None:
                try:
                    post_teardown()
                except BaseException as texc:  # noqa: BLE001
                    if primary is None:
                        primary = texc
            case._outcome = None
        # addCleanup functions run LIFO even when setUp/call/tearDown failed
        # (unittest's doCleanups); the primary exception wins, else the
        # first cleanup error surfaces.
        cleanup_error = None
        while case._cleanups:
            function, args, kwargs = case._cleanups.pop()
            try:
                function(*args, **kwargs)
            except Exception as exc:
                if cleanup_error is None:
                    cleanup_error = exc
        if primary is not None:
            raise primary
        if cleanup_error is not None:
            raise cleanup_error
        if expecting_failure and outcome.expectedFailure is not None:
            # A subtest raised under @expectedFailure: surface it as xfail.
            from pytest._outcomes import XFailed

            expected = outcome.expectedFailure[1]
            if isinstance(expected, XFailed):
                raise expected
            raise XFailed(str(expected) or "expected failure")

    # request.function.__name__ and failure headers show the test method,
    # not this shim.
    run.__name__ = method_name
    run.__qualname__ = f"{cls.__qualname__}.{method_name}"
    run.make_case = make_case
    # The collected TestCase class, for node.cls introspection (reordering
    # plugins shuffle by item.cls.__qualname__). Kept off TestItem.cls so the
    # engine does not instantiate/rebind around the shim runner.
    run.cls = cls
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
