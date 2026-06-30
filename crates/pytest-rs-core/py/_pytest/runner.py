"""Minimal in-process runtest protocol for pytester.getitem items —
upstream runtestprotocol's report shape (setup/call/teardown) with
skip/xfail mark semantics; no fixtures."""

import sys
import traceback

from pytest._outcomes import Exit, Skipped, XFailed
from pytest._raises import ExceptionInfo

from _pytest.reports import CollectReport as CollectReport
from _pytest.reports import TestReport as TestReport
from _pytest.reports import _LongRepr as _LR
from _pytest.skipping import evaluate_skip_marks, evaluate_xfail_marks


class CallInfo:
    """Result/exception of a single phase call (upstream runner.CallInfo):
    `result` is set only on success, `excinfo` only on failure."""

    def __init__(
        self,
        result=None,
        excinfo=None,
        *,
        start=0,
        stop=0,
        duration=0,
        when="call",
        _ispytest=False,
    ):
        self.when = when
        self.excinfo = excinfo
        self.start = start
        self.stop = stop
        self.duration = duration
        if excinfo is None:
            self.result = result

    @classmethod
    def from_call(cls, func, when, reraise=None):
        import time

        excinfo = None
        result = None
        start = time.time()
        precise_start = time.perf_counter()
        try:
            result = func()
        except BaseException:
            excinfo = ExceptionInfo.from_current()
            if reraise is not None and isinstance(excinfo.value, reraise):
                raise
        precise_stop = time.perf_counter()
        duration = precise_stop - precise_start
        stop = time.time()
        return cls(result, excinfo, start=start, stop=stop, duration=duration, when=when)

    def __repr__(self):
        if self.excinfo is None:
            return f"<CallInfo when={self.when!r} result: {self.result!r}>"
        return f"<CallInfo when={self.when!r} excinfo={self.excinfo!r}>"


class _ProtocolReport:
    """The TestReport subset the mark-evaluation tests inspect."""

    def __init__(self, when, outcome, keywords, longrepr=None, sections=()):
        self.when = when
        self.outcome = outcome
        self.keywords = keywords
        if isinstance(longrepr, str) and not isinstance(longrepr, _LR):
            longrepr = _LR(longrepr)
        self.longrepr = longrepr
        self.sections = list(sections)

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

    @property
    def capstdout(self):
        return "".join(c for h, c in self.sections if h.startswith("Captured stdout"))

    @property
    def capstderr(self):
        return "".join(c for h, c in self.sections if h.startswith("Captured stderr"))

    @property
    def caplog(self):
        return "".join(c for h, c in self.sections if h.startswith("Captured log"))

    def __repr__(self):
        return f"<ProtocolReport {self.when!r} outcome={self.outcome!r}>"


class _LogreportSink:
    """Registered in the shim pluginmanager: when a delegated
    pytest_runtest_protocol (pytest-rerunfailures) or a plugin (like
    pytest-subtests) logs via item.ihook.pytest_runtest_logreport, the
    engine records the report so it can render and count it."""

    def __init__(self):
        self._plugin_reports = []

    def pytest_runtest_logreport(self, report):
        capture = globals().get("_native_capture_logreport")
        if capture is not None and capture(report):
            # The report was captured for the engine. The default reporter
            # also received it via shim PM dispatch; track it so
            # subtest_stats() subtracts it from the extra count.
            from pytest._reporter import _track_delegated_report

            _track_delegated_report(report)
            return
        self._plugin_reports.append(report)

    def drain_plugin_reports(self):
        reports = self._plugin_reports
        self._plugin_reports = []
        return reports


_logreport_sink = _LogreportSink()


class _PhaseCapture:
    """Lightweight in-process per-phase capture for the runtestprotocol()
    fallback.  Swaps sys.stdout/stderr for CaptureIO buffers, tracks per-phase
    output, and accumulates sections cumulatively across phases (matching
    upstream pytest semantics: teardown.capstdout includes setup+call output).

    The outer test's global capture (if any) is suspended for the duration so
    the inner item's output does not bleed into the outer report.
    """

    def __init__(self):
        try:
            from pytest._capture import CaptureIO
            from pytest._capture import state as _state

            self._CaptureIO = CaptureIO
            self._global_state = _state
        except Exception:  # noqa: BLE001
            self._CaptureIO = None
            self._global_state = None
        self._out_cap = None
        self._err_cap = None
        self._saved_stdout = None
        self._saved_stderr = None
        # Saved global-capture phase/state so we can restore it after the run.
        self._saved_when = None
        self._saved_installed = False
        # Cumulative (header, text) sections across all phases.
        self._sections: list = []

    def _enabled(self) -> bool:
        return self._CaptureIO is not None

    def start(self) -> None:
        """Suspend outer capture and start fresh sys-level capture."""
        if not self._enabled():
            return
        # Save current sys streams (which may already be the outer capture's
        # CaptureIO tmpfile) so we can restore exactly what was there.
        self._saved_stdout = sys.stdout
        self._saved_stderr = sys.stderr
        # Suspend global capture so the inner item's output doesn't bleed in.
        if self._global_state is not None:
            try:
                self._saved_when = self._global_state.when
                self._saved_installed = self._global_state._installed
                self._global_state.finish_item()
            except Exception:  # noqa: BLE001
                pass
        CaptureIO = self._CaptureIO
        self._out_cap = CaptureIO()
        self._err_cap = CaptureIO()
        sys.stdout = self._out_cap
        sys.stderr = self._err_cap

    def snap_phase(self, when: str) -> list:
        """Drain the current phase's buffer into self._sections and return
        the cumulative sections list up to and including this phase."""
        if not self._enabled() or self._out_cap is None:
            return list(self._sections)
        out = self._out_cap.getvalue()
        err = self._err_cap.getvalue()
        # Truncate buffers for next phase.
        self._out_cap.seek(0)
        self._out_cap.truncate()
        self._err_cap.seek(0)
        self._err_cap.truncate()
        if out:
            self._sections.append((f"Captured stdout {when}", out))
        if err:
            self._sections.append((f"Captured stderr {when}", err))
        return list(self._sections)

    def stop(self) -> None:
        """Stop inner capture and restore the outer streams and capture state."""
        if not self._enabled() or self._out_cap is None:
            return
        # Restore the previous sys streams.
        if self._saved_stdout is not None:
            sys.stdout = self._saved_stdout
        if self._saved_stderr is not None:
            sys.stderr = self._saved_stderr
        self._out_cap = None
        self._err_cap = None
        # Resume the outer global capture if it was installed before.
        if self._global_state is not None and self._saved_installed:
            try:
                self._global_state._capture.resume_capturing()
                self._global_state._installed = True
                if self._global_state.when is None:
                    self._global_state.when = self._saved_when or "call"
            except Exception:  # noqa: BLE001
                pass


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

    keywords = dict(getattr(item, "keywords", None) or {})
    reports = []
    skipped = evaluate_skip_marks(item)
    if skipped is not None:
        reports.append(_ProtocolReport("setup", "skipped", keywords, skipped.reason))
        reports.append(_ProtocolReport("teardown", "passed", keywords))
        return reports

    cap = _PhaseCapture()
    cap.start()
    try:
        # --- setup phase ---
        setup_exc = None
        try:
            # item.setup() handles setup_function + fixture fill (_fillfixtures).
            setup_fn = getattr(item, "setup", None)
            if setup_fn is not None:
                setup_fn()
            else:
                module_setup = getattr(getattr(item, "module", None), "setup_function", None)
                if module_setup is not None:
                    module_setup(item.obj)
        except BaseException as exc:
            setup_exc = exc

        setup_sections = cap.snap_phase("setup")
        if setup_exc is not None:
            if isinstance(setup_exc, Skipped):
                tb = setup_exc.__traceback__
                path, lineno = str(getattr(item, "path", "")), 0
                while tb is not None:
                    path = tb.tb_frame.f_code.co_filename
                    lineno = tb.tb_lineno
                    tb = tb.tb_next
                reason = setup_exc.msg or ""
                reports.append(
                    _ProtocolReport(
                        "setup",
                        "skipped",
                        keywords,
                        (path, lineno, f"Skipped: {reason}"),
                        sections=setup_sections,
                    )
                )
            else:
                reports.append(
                    _ProtocolReport(
                        "setup",
                        "failed",
                        keywords,
                        "".join(traceback.format_exception(setup_exc)),
                        sections=setup_sections,
                    )
                )
            teardown_exc = _run_teardown(item)
            teardown_sections = cap.snap_phase("teardown")
            if teardown_exc is not None:
                reports.append(
                    _ProtocolReport(
                        "teardown",
                        "failed",
                        keywords,
                        "".join(traceback.format_exception(teardown_exc)),
                        sections=teardown_sections,
                    )
                )
            else:
                reports.append(
                    _ProtocolReport("teardown", "passed", keywords, sections=teardown_sections)
                )
            return reports

        reports.append(_ProtocolReport("setup", "passed", keywords, sections=setup_sections))

        # --- call phase ---
        xfailed = evaluate_xfail_marks(item)
        if xfailed and not xfailed.run:
            call = _ProtocolReport(
                "call", "skipped", keywords, "[NOTRUN] " + xfailed.reason, sections=[]
            )
            call.wasxfail = xfailed.reason
        else:
            error = None
            try:
                import inspect

                funcargs = getattr(item, "funcargs", None) or {}
                try:
                    sig = inspect.signature(item.obj)
                    funcargs = {k: v for k, v in funcargs.items() if k in sig.parameters}
                except (ValueError, TypeError):
                    pass
                item.obj(**funcargs)
            except (Exit, KeyboardInterrupt):
                raise
            except BaseException as exc:  # noqa: BLE001 - protocol boundary
                error = exc
            call_sections = cap.snap_phase("call")
            if isinstance(error, Skipped):
                tb = error.__traceback__
                path, lineno = str(getattr(item, "path", "")), 0
                while tb is not None:
                    path = tb.tb_frame.f_code.co_filename
                    lineno = tb.tb_lineno
                    tb = tb.tb_next
                reason = error.msg or ""
                call = _ProtocolReport(
                    "call",
                    "skipped",
                    keywords,
                    (path, lineno, f"Skipped: {reason}"),
                    sections=call_sections,
                )
                reports.append(call)
                _run_teardown(item)
                teardown_sections = cap.snap_phase("teardown")
                reports.append(
                    _ProtocolReport("teardown", "passed", keywords, sections=teardown_sections)
                )
                return reports
            if error is not None:
                if xfailed:
                    call = _ProtocolReport(
                        "call", "skipped", keywords, xfailed.reason, sections=call_sections
                    )
                    call.wasxfail = xfailed.reason
                else:
                    call = _ProtocolReport(
                        "call",
                        "failed",
                        keywords,
                        "".join(traceback.format_exception(error)),
                        sections=call_sections,
                    )
            elif xfailed:
                if xfailed.strict:
                    call = _ProtocolReport(
                        "call",
                        "failed",
                        keywords,
                        "[XPASS(strict)] " + xfailed.reason,
                        sections=call_sections,
                    )
                else:
                    call = _ProtocolReport("call", "passed", keywords, sections=call_sections)
                    call.wasxfail = xfailed.reason
            else:
                call = _ProtocolReport("call", "passed", keywords, sections=call_sections)
        reports.append(call)

        # --- teardown phase ---
        teardown_exc = _run_teardown(item)
        teardown_sections = cap.snap_phase("teardown")
        if teardown_exc is not None:
            reports.append(
                _ProtocolReport(
                    "teardown",
                    "failed",
                    keywords,
                    "".join(traceback.format_exception(teardown_exc)),
                    sections=teardown_sections,
                )
            )
        else:
            reports.append(
                _ProtocolReport("teardown", "passed", keywords, sections=teardown_sections)
            )
    finally:
        cap.stop()
    return reports


def _run_teardown(item) -> BaseException | None:
    """Run teardown_function from the module and drain fixture finalizers.

    Returns the first exception raised during teardown, or None if teardown
    succeeded.  Subsequent exceptions are suppressed so all finalizers run.
    """
    first_exc: BaseException | None = None
    teardown_fn = getattr(getattr(item, "module", None), "teardown_function", None)
    if teardown_fn is not None:
        try:
            teardown_fn(item.obj)
        except BaseException as exc:  # noqa: BLE001
            first_exc = exc
    # Drain yield-fixture finalizers registered via TopRequest.addfinalizer().
    request = getattr(item, "_request", None)
    if request is not None:
        finalizers = getattr(request, "_finalizers", None)
        if finalizers is not None:
            while finalizers:
                fin = finalizers.pop()
                try:
                    fin()
                except BaseException as exc:  # noqa: BLE001
                    if first_exc is None:
                        first_exc = exc
    return first_exc


def collect_one_node(collector):
    """Run collector.collect() and return a CollectReport (upstream API)."""
    try:
        result = list(collector.collect())
        outcome = "passed"
        longrepr = None
    except Exception as exc:
        result = None
        outcome = "failed"
        longrepr = "".join(traceback.format_exception(exc))
    rep = CollectReport(
        nodeid=getattr(collector, "nodeid", ""),
        outcome=outcome,
        longrepr=longrepr,
        result=result,
    )
    path = getattr(collector, "path", None)
    if path is not None:
        rep.location = (str(path.name), None, str(path.name))
    return rep


def pytest_runtest_call(item):
    """Run item.runtest() and store sys.last_* on exception (upstream API)."""
    for attr in ("last_type", "last_value", "last_traceback", "last_exc"):
        try:
            delattr(sys, attr)
        except AttributeError:
            pass
    try:
        item.runtest()
    except Exception as exc:
        sys.last_type = type(exc)
        sys.last_value = exc
        sys.last_traceback = exc.__traceback__
        sys.last_exc = exc
        raise


def pytest_runtest_makereport(item, call):
    """Create a TestReport from an item and CallInfo (upstream default impl)."""
    when = call.when
    wasxfail = None
    if call.excinfo is None:
        outcome = "passed"
        longrepr = None
    else:
        excinfo = call.excinfo
        exc_value = getattr(excinfo, "value", None)
        if isinstance(exc_value, XFailed):
            outcome = "skipped"
            longrepr = exc_value.msg or ""
            wasxfail = "reason: " + (exc_value.msg or "")
        elif isinstance(exc_value, Skipped):
            outcome = "skipped"
            reason = getattr(exc_value, "msg", "") or ""
            path = str(getattr(item, "path", ""))
            longrepr = (path, 0, f"Skipped: {reason}")
        else:
            outcome = "failed"
            longrepr = str(exc_value) if exc_value is not None else str(excinfo)
    location = getattr(item, "location", None)
    if location is None:
        path = str(getattr(item, "path", "") or "")
        location = (path, getattr(item, "lineno", None), getattr(item, "name", ""))
    report = TestReport(
        nodeid=getattr(item, "nodeid", ""),
        when=when,
        outcome=outcome,
        longrepr=longrepr,
        location=location,
        keywords=dict(getattr(item, "keywords", None) or {}),
        duration=getattr(call, "duration", 0),
        start=getattr(call, "start", 0),
        stop=getattr(call, "stop", 0),
    )
    if wasxfail is not None:
        report.wasxfail = wasxfail
    return report


def check_interactive_exception(call, report):
    """Check whether the call raised an exception that should be reported as interactive."""
    import bdb

    if call.excinfo is None:
        return False
    if hasattr(report, "wasxfail"):
        return False
    if isinstance(call.excinfo.value, (Skipped, bdb.BdbQuit)):
        return False
    return True


from _pytest._stub import __getattr__  # noqa: E402, F401
