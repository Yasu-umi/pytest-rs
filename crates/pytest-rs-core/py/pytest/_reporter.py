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
from typing import Any

from pytest._pluginmanager import _accepted_kwargs, pluginmanager

_default: Any = None


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


def setup(config: Any) -> None:
    """Register the default terminalreporter (called before the engine
    fires pytest_configure into Python plugins)."""
    global _default
    if _default is not None:
        return
    from _pytest.terminal import TerminalReporter

    pluginmanager.register(_CoreHeader(), "_core_report_header")
    _default = TerminalReporter(config)
    pluginmanager.register(_default, "terminalreporter")


def replacement() -> Any | None:
    """The non-default 'terminalreporter' plugin, or None when terminal
    output stays native."""
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
    if reporter is None:
        return
    _call(reporter, "pytest_runtest_logstart", nodeid=nodeid, location=location)


def logreport(report: Any) -> None:
    reporter = replacement()
    if reporter is None:
        return
    _call(reporter, "pytest_runtest_logreport", report=report)


def logfinish(nodeid: str, location: tuple) -> None:
    reporter = replacement()
    if reporter is None:
        return
    _call(reporter, "pytest_runtest_logfinish", nodeid=nodeid, location=location)


def collectreport(report: Any) -> None:
    reporter = replacement()
    if reporter is not None:
        _call(reporter, "pytest_collectreport", report=report)
    # Also dispatch to instance-registered plugins (e.g., relay plugin).
    from pytest._pluginmanager import instance_hook_impls
    for impl in instance_hook_impls("pytest_collectreport"):
        try:
            impl(report=report)
        except Exception:
            pass


def _feed_warnings(reporter: Any) -> None:
    """Mirror captured warnings into reporter.stats['warnings'] (upstream
    feeds them live via pytest_warning_recorded)."""
    import warnings as _warnings

    try:
        from _pytest.terminal import WarningReport

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


def feed_default(report: Any) -> None:
    """Feed a report to the default reporter's stats without printing terminal
    output. Used in native mode so conftest pytest_terminal_summary hooks can
    access terminalreporter.stats['passed'] etc."""
    if _default is None:
        return
    try:
        category, _, _ = _default._gettestkindstatus(report)
        _default._add_stats(category, [report])
    except Exception:
        pass


def finish(session: Any, exitstatus: int, shouldfail: str | None = None) -> None:
    """End-of-run summaries, in upstream's pytest_terminal_summary /
    pytest_sessionfinish wrapper order (the sessionfinish wrapper closes the
    progress line first, then summaries, then the stats line)."""
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
