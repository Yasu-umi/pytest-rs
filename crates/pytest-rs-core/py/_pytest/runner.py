"""Minimal in-process runtest protocol for pytester.getitem items —
upstream runtestprotocol's report shape (setup/call/teardown) with
skip/xfail mark semantics; no fixtures."""

import traceback


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

    def __repr__(self):
        return f"<ProtocolReport {self.when!r} outcome={self.outcome!r}>"


class _LogreportSink:
    """Registered in the shim pluginmanager: when a delegated
    pytest_runtest_protocol (pytest-rerunfailures) logs via
    item.ihook.pytest_runtest_logreport, the engine records the report so it
    can render and count it. A no-op outside a delegated run."""

    def pytest_runtest_logreport(self, report):
        _native_capture_logreport(report)


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

    keywords = dict(getattr(item, "keywords", None) or {})
    reports = []
    skipped = evaluate_skip_marks(item)
    if skipped is not None:
        reports.append(_ProtocolReport("setup", "skipped", keywords, skipped.reason))
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
        except BaseException as exc:  # noqa: BLE001 - protocol boundary
            error = exc
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
    reports.append(_ProtocolReport("teardown", "passed", keywords))
    return reports


from _pytest._stub import __getattr__  # noqa: E402, F401
