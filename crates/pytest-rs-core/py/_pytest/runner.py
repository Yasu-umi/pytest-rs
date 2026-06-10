"""Minimal in-process runtest protocol for pytester.getitem items —
upstream runtestprotocol's report shape (setup/call/teardown) with
skip/xfail mark semantics; no fixtures."""

import traceback

from _pytest.reports import CollectReport as CollectReport
from _pytest.reports import TestReport as TestReport


class CallInfo:
    """Result/exception of a single phase call (upstream runner.CallInfo):
    `result` is set only on success, `excinfo` only on failure."""

    def __init__(self, when, result, excinfo):
        self.when = when
        self.excinfo = excinfo
        if excinfo is None:
            self.result = result

    @classmethod
    def from_call(cls, func, when, reraise=None):
        from pytest._raises import ExceptionInfo

        excinfo = None
        result = None
        try:
            result = func()
        except BaseException:
            excinfo = ExceptionInfo.from_current()
            if reraise is not None and isinstance(excinfo.value, reraise):
                raise
        return cls(when, result, excinfo)

    def __repr__(self):
        if self.excinfo is None:
            return f"<CallInfo when={self.when!r} result: {self.result!r}>"
        return f"<CallInfo when={self.when!r} excinfo={self.excinfo!r}>"


class _ProtocolReport:
    """The TestReport subset the mark-evaluation tests inspect."""

    def __init__(self, when, outcome, keywords, longrepr=None):
        self.when = when
        self.outcome = outcome
        self.keywords = keywords
        self.longrepr = longrepr

    @property
    def passed(self):
        return self.outcome == "passed"

    @property
    def failed(self):
        return self.outcome == "failed"

    @property
    def skipped(self):
        return self.outcome == "skipped"

    @property
    def longreprtext(self):
        """The longrepr rendered as text (upstream TestReport.longreprtext):
        a skip's 3-tuple yields its reason, a string is returned as-is."""
        longrepr = self.longrepr
        if longrepr is None:
            return ""
        if isinstance(longrepr, tuple):
            return longrepr[2]
        return str(longrepr)

    # The lightweight protocol shim does not capture output.
    capstdout = ""
    capstderr = ""
    caplog = ""

    def __repr__(self):
        return f"<ProtocolReport {self.when!r} outcome={self.outcome!r}>"


class _LogreportSink:
    """Registered in the shim pluginmanager: when a delegated
    pytest_runtest_protocol (pytest-rerunfailures) logs via
    item.ihook.pytest_runtest_logreport, the engine records the report so it
    can render and count it. A no-op outside a delegated run."""

    def pytest_runtest_logreport(self, report):
        capture = globals().get("_native_capture_logreport")
        if capture is not None:
            capture(report)


def runtestprotocol(item, log=True, nextitem=None):
    # Inside a delegated protocol (a plugin replacing pytest_runtest_protocol),
    # run the real engine phases and return _pytest.reports proxies. Outside a
    # run (pytester.getitem mark-eval tests), fall back to the lightweight shim.
    runner = globals().get("_native_run_item_phases")
    if runner is not None:
        try:
            reports = runner()
        except RuntimeError:
            reports = None
        if reports is not None:
            if log:
                for report in reports:
                    item.ihook.pytest_runtest_logreport(report=report)
            return reports

    from _pytest.skipping import evaluate_skip_marks, evaluate_xfail_marks
    from pytest._outcomes import Exit, Skipped

    keywords = dict(getattr(item, "keywords", None) or {})
    reports = []
    skipped = evaluate_skip_marks(item)
    if skipped is not None:
        reports.append(_ProtocolReport("setup", "skipped", keywords, skipped.reason))
        reports.append(_ProtocolReport("teardown", "passed", keywords))
        return reports

    # Call setup_function from the test module if present.
    setup_fn = getattr(getattr(item, "module", None), "setup_function", None)
    if setup_fn is not None:
        try:
            setup_fn(item.obj)
        except BaseException as setup_exc:
            if isinstance(setup_exc, Skipped):
                tb = setup_exc.__traceback__
                path, lineno = str(getattr(item, "path", "")), 0
                while tb is not None:
                    path = tb.tb_frame.f_code.co_filename
                    lineno = tb.tb_lineno
                    tb = tb.tb_next
                reason = setup_exc.msg or ""
                reports.append(
                    _ProtocolReport("setup", "skipped", keywords, (path, lineno, f"Skipped: {reason}"))
                )
            else:
                reports.append(
                    _ProtocolReport("setup", "failed", keywords, "".join(traceback.format_exception(setup_exc)))
                )
            reports.append(_ProtocolReport("teardown", "passed", keywords))
            return reports

    reports.append(_ProtocolReport("setup", "passed", keywords))

    xfailed = evaluate_xfail_marks(item)
    if xfailed and not xfailed.run:
        call = _ProtocolReport("call", "skipped", keywords, "[NOTRUN] " + xfailed.reason)
        call.wasxfail = xfailed.reason
    else:
        error = None
        try:
            item.obj()
        except (Exit, KeyboardInterrupt):
            raise
        except BaseException as exc:  # noqa: BLE001 - protocol boundary
            error = exc
        if isinstance(error, Skipped):
            # pytest.skip() inside the body: a skipped call report whose
            # longrepr is the (path, lineno, "Skipped: reason") tuple.
            tb = error.__traceback__
            path, lineno = str(getattr(item, "path", "")), 0
            while tb is not None:
                path = tb.tb_frame.f_code.co_filename
                lineno = tb.tb_lineno
                tb = tb.tb_next
            reason = error.msg or ""
            call = _ProtocolReport(
                "call", "skipped", keywords, (path, lineno, f"Skipped: {reason}")
            )
            reports.append(call)
            reports.append(_ProtocolReport("teardown", "passed", keywords))
            return reports
        if error is not None:
            if xfailed:
                call = _ProtocolReport("call", "skipped", keywords, xfailed.reason)
                call.wasxfail = xfailed.reason
            else:
                call = _ProtocolReport(
                    "call",
                    "failed",
                    keywords,
                    "".join(traceback.format_exception(error)),
                )
        elif xfailed:
            if xfailed.strict:
                call = _ProtocolReport(
                    "call", "failed", keywords, "[XPASS(strict)] " + xfailed.reason
                )
            else:
                call = _ProtocolReport("call", "passed", keywords)
                call.wasxfail = xfailed.reason
        else:
            call = _ProtocolReport("call", "passed", keywords)
    reports.append(call)

    # Call teardown_function from the test module if present.
    teardown_fn = getattr(getattr(item, "module", None), "teardown_function", None)
    if teardown_fn is not None:
        try:
            teardown_fn(item.obj)
        except BaseException as teardown_exc:
            reports.append(
                _ProtocolReport("teardown", "failed", keywords, "".join(traceback.format_exception(teardown_exc)))
            )
            return reports
    reports.append(_ProtocolReport("teardown", "passed", keywords))
    return reports


from _pytest._stub import __getattr__  # noqa: E402, F401
