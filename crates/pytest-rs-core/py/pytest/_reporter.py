"""Terminal-reporter delegation bridge.

pytest-rs renders terminal output natively (in Rust). Plugins like
pytest-sugar / pytest-pretty replace the 'terminalreporter' plugin during
pytest_configure; when that happens the engine suppresses its native output
and drives the replacement object through the same hook calls upstream
pluggy would make. The default TerminalReporter registered here is the
sacrificial stand-in those plugins unregister.
"""

from __future__ import annotations

import sys
import traceback
import warnings as _warnings
from typing import Any

from _pytest.terminal import TerminalReporter, WarningReport, _default_teststatus

from pytest._pluginmanager import _accepted_kwargs, instance_hook_impls, pluginmanager

_default: Any = None


def _trylast(func):
    """Mark a hook impl to dispatch last (pluggy trylast) without importing
    pytest here — the relay reads `pytest_impl`."""
    func.pytest_impl = {"trylast": True}
    return func


class _CoreHeader:
    """The rootdir/plugins header lines upstream's built-in
    pytest_report_header impls contribute (reporter-replacing plugins call
    config.hook.pytest_report_header and expect them)."""

    @staticmethod
    def pytest_report_header(config: Any) -> list[str]:
        lines = []
        try:
            verbose = int(getattr(config.option, "verbose", 0) or 0)
        except Exception:
            verbose = 0
        if verbose > 0:
            cache_dir = None
            try:
                cache_dir = config.getini("cache_dir")
            except Exception:
                pass
            lines.append(f"cachedir: {cache_dir or '.pytest_cache'}")
        rootpath = getattr(config, "rootpath", None)
        if rootpath is not None:
            lines.append(f"rootdir: {rootpath}")
        plugins = []
        try:
            from importlib.metadata import distributions

            registered_modules = {
                getattr(plugin, "__name__", None) for plugin in pluginmanager._plugins
            }
            for dist in distributions():
                for ep in dist.entry_points:
                    if ep.group != "pytest11":
                        continue
                    module = ep.value.split(":")[0].strip()
                    if module not in registered_modules:
                        continue
                    name = f"{dist.metadata['Name']}-{dist.version}"
                    if name.startswith("pytest-"):
                        name = name[7:]
                    if name not in plugins:
                        plugins.append(name)
        except Exception:
            pass
        if plugins:
            lines.append("plugins: {}".format(", ".join(sorted(plugins))))
        return lines


class _CoreTestStatus:
    """The (category, letter, word) default upstream's _pytest.runner /
    _pytest.skipping pytest_report_teststatus impls provide. Registered first
    so it dispatches last (the relay runs registered plugins LIFO), letting
    plugin impls win the firstresult. Without it, plugins that call
    config.hook.pytest_report_teststatus directly (pytest-bdd's
    gherkin-terminal-reporter) get None and crash unpacking it."""

    @staticmethod
    @_trylast
    def pytest_report_teststatus(report: Any, config: Any) -> Any:
        return _default_teststatus(report)


class _CorePyCollectMakeModule:
    """The default pytest_pycollect_makemodule impl upstream's _pytest.python
    provides: return a live Module collector node whose `.obj` is the imported
    module. Registered as a plain trylast impl so conftest hookwrappers
    (@pytest.hookimpl(wrapper=True)) surround it — a wrapper that mutates
    `mod.obj` (issue #205) thus mutates the real module the test functions read
    their globals from. The native engine fires this relay only when a conftest
    actually provides a makemodule hook, so default collection is unchanged."""

    @staticmethod
    @_trylast
    def pytest_pycollect_makemodule(module_path: Any, parent: Any) -> Any:
        from pytest._node import File

        node: Any = File(name=module_path.name, path=module_path, parent=parent)
        node.obj = getattr(parent, "_rs_module", None)
        node._rs_default_makemodule = True
        return node


class _CallInfo:
    """Minimal _pytest.runner.CallInfo stand-in for pytest_runtest_makereport
    consumers (pytest-bdd reads only item/report; others may read when)."""

    def __init__(self, when, excinfo=None):
        self.when = when
        self.excinfo = excinfo
        self.result = None
        self.start = 0.0
        self.stop = 0.0
        self.duration = 0.0


_makereport_result: Any = None


class _CoreMakeReport:
    """The plain (firstresult) pytest_runtest_makereport impl upstream's
    _pytest.runner provides: it returns the report the engine already built,
    which registered hookwrappers (pytest-bdd attaching .scenario) post-process
    before pytest_runtest_logreport. Registered first so it dispatches last."""

    @staticmethod
    @_trylast
    def pytest_runtest_makereport(item: Any, call: Any) -> Any:
        return _makereport_result


def run_makereport(report: Any, node: Any, when: str) -> Any:
    """Drive registered pytest_runtest_makereport hookwrappers over `report`
    so plugins can enrich it (pytest-bdd's .scenario) before it is logged."""
    global _makereport_result
    _makereport_result = report
    try:
        pluginmanager.hook.pytest_runtest_makereport(item=node, call=_CallInfo(when))
    finally:
        _makereport_result = None
    return report


def setup(config: Any) -> None:
    """Register the default terminalreporter (called before the engine
    fires pytest_configure into Python plugins)."""
    global _default
    if _default is not None:
        _default.__init__(config)
        return
    pluginmanager.register(_CoreHeader(), "_core_report_header")
    pluginmanager.register(_CoreTestStatus(), "_core_teststatus")
    pluginmanager.register(_CoreMakeReport(), "_core_makereport")
    pluginmanager.register(_CorePyCollectMakeModule(), "_core_pycollect_makemodule")
    _default = TerminalReporter(config)
    pluginmanager.register(_default, "terminalreporter")


def replacement() -> Any | None:
    """The non-default 'terminalreporter' plugin, or None when terminal
    output stays native."""
    # When setup() was never called (_default is None) — e.g. a -n worker,
    # whose only stdout is the IPC pipe — there is no pytest-rs-managed
    # default and no replacement to drive; the registered TerminalReporter
    # is the upstream default, which must stay silent (the master renders).
    if _default is None:
        return None
    reporter = pluginmanager.getplugin("terminalreporter")
    if reporter is None or reporter is _default:
        return None
    return reporter


def _call(obj: Any, name: str, /, **kwargs: Any) -> None:
    func = getattr(obj, name, None)
    if not callable(func):
        return
    try:
        func(**_accepted_kwargs(func, kwargs))
    except Exception:
        sys.stderr.write(f"INTERNALERROR in terminalreporter.{name}\n")
        traceback.print_exc()


# ---- engine-driven hook calls (no-ops unless a replacement registered) ----


def sessionstart(session: Any) -> None:
    """Drive the replacement's pytest_sessionstart: it owns the session
    header (the engine skipped its native one)."""
    reporter = replacement()
    if reporter is None:
        return
    reporter._session = session
    _call(reporter, "pytest_sessionstart", session=session)


def collection_finish(session: Any, numcollected: int) -> None:
    """The 'collected N items' line: the replacement's
    pytest_collection_finish (the base report_collect needs _numcollected,
    which native collection never fed it)."""
    reporter = replacement()
    if reporter is None:
        return
    reporter._session = session
    reporter._numcollected = numcollected
    _call(reporter, "pytest_collection_finish", session=session)


def deselected(items: list) -> None:
    reporter = replacement()
    if reporter is None:
        return
    _call(reporter, "pytest_deselected", items=items)


def logstart(nodeid: str, location: tuple) -> None:
    reporter = replacement()
    if reporter is not None:
        _call(reporter, "pytest_runtest_logstart", nodeid=nodeid, location=location)
    for impl in instance_hook_impls("pytest_runtest_logstart"):
        try:
            impl(nodeid=nodeid, location=location)
        except Exception:
            pass


def logreport(report: Any) -> None:
    reporter = replacement()
    if reporter is not None:
        _call(reporter, "pytest_runtest_logreport", report=report)
    for impl in instance_hook_impls("pytest_runtest_logreport"):
        try:
            impl(report=report)
        except Exception:
            pass


def logfinish(nodeid: str, location: tuple) -> None:
    reporter = replacement()
    if reporter is not None:
        _call(reporter, "pytest_runtest_logfinish", nodeid=nodeid, location=location)
    for impl in instance_hook_impls("pytest_runtest_logfinish"):
        try:
            impl(nodeid=nodeid, location=location)
        except Exception:
            pass


def collectreport(report: Any) -> None:
    reporter = replacement()
    if reporter is not None:
        _call(reporter, "pytest_collectreport", report=report)
    # Also dispatch to instance-registered plugins (e.g., relay plugin).
    for impl in instance_hook_impls("pytest_collectreport"):
        try:
            impl(report=report)
        except Exception:
            pass


def _feed_warnings(reporter: Any) -> None:
    """Mirror captured warnings into reporter.stats['warnings'] (upstream
    feeds them live via pytest_warning_recorded)."""
    try:
        from pytest import _wcapture

        stats = getattr(reporter, "stats", None)
        if stats is None or stats.get("warnings"):
            return
        entries = []
        for warning in _wcapture.captured:
            message = _warnings.formatwarning(
                warning["message"],  # type: ignore[arg-type]
                warning["category"],  # type: ignore[arg-type]
                warning["filename"],  # type: ignore[arg-type]
                warning["lineno"],  # type: ignore[arg-type]
            )
            entries.append(
                WarningReport(
                    message,
                    nodeid=warning["test"],
                    fslocation=(warning["filename"], warning["lineno"]),
                )
            )
        if entries:
            stats["warnings"] = entries
    except Exception:
        pass


_fed_counts: dict[str, int] = {}


def _track_delegated_report(report: Any) -> None:
    """Account for a report the default reporter received via the shim PM
    during a delegated protocol.  Incrementing _fed_counts prevents
    subtest_stats() from returning it as an 'extra' count (the engine
    already tracks it in session.reports)."""
    if _default is None:
        return
    try:
        category, _, _ = _default._gettestkindstatus(report)
        _fed_counts[category] = _fed_counts.get(category, 0) + 1
    except Exception:
        pass


def feed_default(report: Any) -> None:
    """Feed a report to the default reporter's stats without printing terminal
    output. Used in native mode so conftest pytest_terminal_summary hooks can
    access terminalreporter.stats['passed'] etc."""
    if _default is None:
        return
    try:
        category, _, _ = _default._gettestkindstatus(report)
        _default._add_stats(category, [report])
        _fed_counts[category] = _fed_counts.get(category, 0) + 1
    except Exception:
        pass


def ensure_newline() -> None:
    """Ensure the default reporter has a trailing newline (plugins may
    have written partial lines through the relay)."""
    if _default is not None:
        _default.ensure_newline()


def subtest_stats() -> dict[str, int]:
    """Return plugin-driven stat counts from the default reporter that
    the native engine does not see (reports emitted through the hook
    relay, not through session.reports). Subtracts counts fed by the
    engine via feed_default to avoid double-counting."""
    if _default is None:
        return {}
    result = {}
    for key, items in _default.stats.items():
        if not items or key in ("", "warnings", "deselected", "error"):
            continue
        extra = len(items) - _fed_counts.get(key, 0)
        if extra > 0:
            result[key] = extra
    return result


def finish(session: Any, exitstatus: int, shouldfail: str | None = None) -> None:
    """End-of-run summaries, in upstream's pytest_terminal_summary /
    pytest_sessionfinish wrapper order (the sessionfinish wrapper closes the
    progress line first, then summaries, then the stats line)."""
    # Fire pytest_sessionfinish on instance-registered plugins (e.g., relay plugin).
    for impl in instance_hook_impls("pytest_sessionfinish"):
        try:
            impl(session=session, exitstatus=exitstatus)
        except Exception:
            pass
    reporter = replacement()
    if reporter is None:
        return
    _feed_warnings(reporter)
    try:
        reporter._tw.line("")
    except Exception:
        pass
    _call(reporter, "summary_errors")
    _call(reporter, "summary_failures")
    _call(reporter, "summary_xfailures")
    _call(reporter, "summary_warnings")
    _call(reporter, "summary_passes")
    _call(reporter, "summary_xpasses")
    # Other plugins' pytest_terminal_summary impls (the replacement itself
    # defines none — upstream's is a hookwrapper this sequence replays).
    try:
        pluginmanager.hook.pytest_terminal_summary(
            terminalreporter=reporter, exitstatus=exitstatus, config=reporter.config
        )
    except Exception:
        traceback.print_exc()
    _call(reporter, "short_test_summary")
    _call(reporter, "summary_warnings")
    if shouldfail:
        _call(reporter, "write_sep", sep="!", title=str(shouldfail), red=True)
    _call(reporter, "summary_stats")
