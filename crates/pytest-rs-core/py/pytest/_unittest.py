"""unittest.TestCase integration: build zero-arg runners per test method."""

import unittest

from pytest._outcomes import Skipped


def is_testcase_class(obj):
    try:
        return isinstance(obj, type) and issubclass(obj, unittest.TestCase)
    except TypeError:
        return False


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


# Entries recorded by _ResultCollector.addError/addFailure/addSkip/... beyond
# the first one for the item currently under test: unittest.TestCase.run()
# may independently capture setUp, test-body, tearDown, and addCleanup
# failures all in one `case(result=...)` call (the engine's single "call"
# phase). Only the first becomes the call-phase exception (raised directly);
# these extras are drained by the engine's teardown phase (see
# python::pop_unittest_extra_errors / crates/.../runner/teardown.rs),
# mirroring upstream's `TestCaseFunction._excinfo` landing on whichever
# phase's makereport call comes next.
_extra_errors: list[BaseException] = []


def pop_extra_errors():
    out = list(_extra_errors)
    _extra_errors.clear()
    return out


def _wrap_excinfo_or_fallback(rawexcinfo):
    """Wrap a raw (type, value, traceback) exc_info into an ExceptionInfo,
    falling back to a synthetic NOTE:/ERROR: Failed (as an ExceptionInfo of
    itself, upstream's own fallback shape) when it can't be wrapped — e.g.
    `_pytest._code.ExceptionInfo` was monkeypatched to something
    incompatible, or rawexcinfo isn't a real 3-tuple at all. Always returns
    an ExceptionInfo; never raises."""
    import _pytest._code

    try:
        excinfo = _pytest._code.ExceptionInfo.from_exc_info(rawexcinfo)
        # Invoke the attributes to trigger storing the traceback; a
        # monkeypatched/incompatible ExceptionInfo raises here too.
        _ = excinfo.value
        _ = excinfo.traceback
        return excinfo
    except TypeError:
        import traceback

        from pytest._outcomes import fail

        try:
            try:
                values = traceback.format_exception(*rawexcinfo)
                values.insert(
                    0,
                    "NOTE: Incompatible Exception Representation, displaying natively:\n\n",
                )
                fail("".join(values), pytrace=False)
            except (fail.Exception, KeyboardInterrupt):
                raise
            except BaseException:
                fail(
                    f"ERROR: Unknown Incompatible Exception representation:\n{rawexcinfo!r}",
                    pytrace=False,
                )
        except fail.Exception:
            return _pytest._code.ExceptionInfo.from_current()


class TestCaseFunction:
    """Upstream TestCaseFunction's full result-callback protocol as a mixin
    — needed not just for code that introspects a unittest item directly
    (pytester.getitems() + isinstance/addError calls,
    test_testcase_totally_incompatible_exception_info) but also because
    third-party plugins (e.g. pytest-subtests) monkeypatch individual
    methods on the real _pytest.unittest.TestCaseFunction class at
    pytest_configure time (`TestCaseFunction._originaladdSkip =
    TestCaseFunction.addSkip`) and need the full upstream method set to
    already exist as class attributes, independent of whether pytest-rs's
    engine ever calls them. NOT used by the engine's main
    collection/execution path, which drives unittest tests via
    make_runner()'s _ResultCollector instead (a plain Function item, not
    this class)."""

    _excinfo = None

    def startTest(self, testcase):
        pass

    def stopTest(self, testcase):
        pass

    def addSuccess(self, testcase):
        pass

    def addDuration(self, testcase, elapsed):
        pass

    def addError(self, testcase, rawexcinfo):
        from pytest._outcomes import exit as _exit

        try:
            if isinstance(rawexcinfo[1], _exit.Exception):
                _exit(rawexcinfo[1].msg)
        except TypeError:
            pass
        self._addexcinfo(rawexcinfo)

    def addFailure(self, testcase, rawexcinfo):
        self._addexcinfo(rawexcinfo)

    def addSkip(self, testcase, reason):
        import sys

        from pytest._outcomes import skip

        try:
            skip(str(reason))
        except skip.Exception:
            self._addexcinfo(sys.exc_info())

    def addExpectedFailure(self, testcase, rawexcinfo, reason=""):
        import sys

        from pytest._outcomes import xfail

        try:
            xfail(str(reason))
        except xfail.Exception:
            self._addexcinfo(sys.exc_info())

    def addUnexpectedSuccess(self, testcase, reason=None):
        import sys

        from pytest._outcomes import fail

        msg = "Unexpected success"
        if reason:
            msg += f": {reason.reason}"
        try:
            fail(msg, pytrace=False)
        except fail.Exception:
            self._addexcinfo(sys.exc_info())

    def addSubTest(self, test_case, test, exc_info):
        if exc_info is not None:
            self._addexcinfo(exc_info)

    def _addexcinfo(self, rawexcinfo):
        excinfo = _wrap_excinfo_or_fallback(rawexcinfo)
        if self._excinfo is None:
            self._excinfo = []
        self._excinfo.append(excinfo)


class _ResultCollector:
    """The `result` object passed to `case(result=...)` (TestCase.__call__),
    replacing upstream's TestCaseFunction. unittest.TestCase.run() drives
    setUp/body/tearDown/cleanup itself and calls back into this object once
    per independently-captured outcome — this just records them in
    `self.entries` (occurrence order) instead of reporting through pytest
    hooks directly, so make_runner()'s run() can raise entries[0] as the
    call-phase exception and stash the rest as extras (see _extra_errors
    above). Subtest results (addSubTest / a subtest's addSkip) are routed
    into the pytest._subtests accumulator, same as the old _SubtestRecorder
    (records tagged "unittest" — those failures do not fail the enclosing
    test)."""

    def __init__(self, case) -> None:
        self._case = case
        self.failfast = False
        self.entries: list[BaseException] = []

    # -- whole-test protocol (upstream TestCaseFunction) -----------------

    def startTest(self, test) -> None:
        pass

    def stopTest(self, test) -> None:
        pass

    def addSuccess(self, test) -> None:
        pass

    def addDuration(self, test, elapsed) -> None:
        pass

    def _addexcinfo(self, rawexcinfo) -> None:
        excinfo = _wrap_excinfo_or_fallback(rawexcinfo)
        self.entries.append(excinfo.value)

    def addError(self, test, rawexcinfo) -> None:
        from pytest._outcomes import exit as _exit

        try:
            if isinstance(rawexcinfo[1], _exit.Exception):  # type: ignore[attr-defined]
                _exit(rawexcinfo[1].msg)
        except TypeError:
            pass
        self._addexcinfo(rawexcinfo)

    def addFailure(self, test, rawexcinfo) -> None:
        self._addexcinfo(rawexcinfo)

    def addSkip(self, test, reason) -> None:
        from unittest.case import _SubTest  # type: ignore[attr-defined]

        if isinstance(test, _SubTest):
            self._record(
                self._subtest_desc(test),
                "skipped",
                reason=str(reason),
                location=self._case_location(),
            )
            return
        method = getattr(type(self._case), getattr(self._case, "_testMethodName", ""), None)
        self.entries.append(_skipped_at(str(reason), method))

    def addExpectedFailure(self, test, rawexcinfo, reason="") -> None:
        from pytest._outcomes import xfail

        try:
            xfail(str(reason))
        except xfail.Exception as e:  # type: ignore[attr-defined]
            self.entries.append(e)

    def addUnexpectedSuccess(self, test, reason=None) -> None:
        from pytest._outcomes import Failed

        msg = "Unexpected success"
        if reason:
            msg += f": {reason.reason}"
        failure = Failed(msg=msg)
        # Upstream: bare message, no traceback.
        failure.pytrace = False  # type: ignore[attr-defined]
        self.entries.append(failure)

    # -- subtest protocol (unchanged from the old _SubtestRecorder) ------

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
            from pytest._debugging import maybe_interact

            maybe_interact(test_case, exc_info[1])

    def _case_location(self):
        import os

        method = getattr(type(self._case), getattr(self._case, "_testMethodName", ""), None)
        code = getattr(method, "__code__", None)
        if code is None:
            return None
        filename = code.co_filename
        try:
            rel = os.path.relpath(filename)
            if not rel.startswith(".."):
                filename = rel
        except ValueError:
            pass
        return f"{filename}:{code.co_firstlineno}"


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
        # Class-level skip decorators: short-circuit before ever driving the
        # TestCase.run() protocol below (upstream's own run() would reach the
        # same outcome via its internal __unittest_skip__ check and
        # result.addSkip, but skipping the case(result=...) call entirely
        # here is cheaper and matches this shim's prior behavior exactly).
        if getattr(cls, "__unittest_skip__", False):
            raise _skipped_at(getattr(cls, "__unittest_skip_why__", ""), method)
        if getattr(method, "__unittest_skip__", False):
            raise _skipped_at(getattr(method, "__unittest_skip_why__", ""), method)

        # Let unittest.TestCase.__call__ (-> .run()) drive setUp/body/
        # tearDown/cleanup itself: this is what makes a subclass's own
        # overridden run()/__call__ (Django's SimpleTestCase pre_setup/
        # post_teardown, IsolatedAsyncioTestCase's asyncio runner) apply for
        # free, and captures setUp/body/tearDown/cleanup failures
        # independently (matching upstream's _excinfo semantics) instead of
        # one clobbering another via Python's finally-reraise.
        result = _ResultCollector(case)
        case(result=result)
        entries = result.entries
        if entries:
            _extra_errors.extend(entries[1:])
            raise entries[0]

    # request.function.__name__ and failure headers show the test method,
    # not this shim.
    run.__name__ = method_name
    run.__qualname__ = f"{cls.__qualname__}.{method_name}"
    run.make_case = make_case
    # The collected TestCase class, for node.cls introspection (reordering
    # plugins shuffle by item.cls.__qualname__). Kept off TestItem.cls so the
    # engine does not instantiate/rebind around the shim runner.
    run.cls = cls
    # Copy plugin-visible attributes from the original method so hooks that
    # inspect item.obj (e.g. pytest-django's @tag → marker conversion) work.
    original_method = getattr(cls, method_name, None)
    if original_method is not None:
        for _attr in ("tags",):
            _val = getattr(original_method, _attr, None)
            if _val is not None:
                setattr(run, _attr, _val)
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
