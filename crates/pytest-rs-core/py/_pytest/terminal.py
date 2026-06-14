"""Terminal-reporting helpers ported from pytest's _pytest/terminal.py.

Two consumers:
- upstream unit-tests exercise the summary-stats logic directly;
- reporter-replacing plugins (pytest-sugar, pytest-pretty) subclass
  TerminalReporter and rely on its writer/progress/summary surface. The
  engine renders natively unless such a plugin registers itself as the
  'terminalreporter' plugin; then it drives the replacement through
  pytest._reporter, so the base-class behavior here is what those plugins
  inherit.
"""

import datetime
import os
import platform
import sys
import textwrap
import time
from functools import partial
from pathlib import Path

from _pytest._io import TerminalWriter
from _pytest._io.wcwidth import wcswidth
from _pytest.compat import running_on_ci


def _plugin_nameversions(plugininfo):
    values = []
    for plugin, dist in plugininfo:
        # Gets us name and version!
        name = f"{dist.project_name}-{dist.version}"
        # Questionable convenience, but it keeps things short.
        if name.startswith("pytest-"):
            name = name[7:]
        # Plugins are printed by python package name; a package can have
        # more than one plugin.
        if name not in values:
            values.append(name)
    return values


def format_session_duration(seconds):
    """Format the given seconds in a human readable manner to show in the
    final summary."""
    if seconds < 60:
        return f"{seconds:.2f}s"
    else:
        dt = datetime.timedelta(seconds=int(seconds))
        return f"{seconds:.2f}s ({dt})"


def format_node_duration(seconds):
    """Format the given seconds in a human readable manner to show in the
    test progress."""
    # The formatting is designed to be compact and readable, with at most
    # 7 characters for durations below 100 hours.
    if seconds < 0.00001:
        return f" {seconds * 1000000:.3f}us"
    if seconds < 0.0001:
        return f" {seconds * 1000000:.2f}us"
    if seconds < 0.001:
        return f" {seconds * 1000000:.1f}us"
    if seconds < 0.01:
        return f" {seconds * 1000:.3f}ms"
    if seconds < 0.1:
        return f" {seconds * 1000:.2f}ms"
    if seconds < 1:
        return f" {seconds * 1000:.1f}ms"
    if seconds < 60:
        return f" {seconds:.3f}s"
    if seconds < 3600:
        return f" {seconds // 60:.0f}m {seconds % 60:.0f}s"
    return f" {seconds // 3600:.0f}h {(seconds % 3600) // 60:.0f}m"


KNOWN_TYPES = (
    "failed",
    "passed",
    "skipped",
    "deselected",
    "xfailed",
    "xpassed",
    "warnings",
    "error",
    "subtests passed",
    "subtests failed",
    "subtests skipped",
)

_color_for_type = {
    "failed": "red",
    "error": "red",
    "warnings": "yellow",
    "passed": "green",
    "subtests passed": "green",
    "subtests failed": "red",
}
_color_for_type_default = "yellow"


def pluralize(count, noun):
    # No need to pluralize words such as `failed` or `passed`.
    if noun not in ["error", "warnings", "test"]:
        return count, noun

    # The `warnings` key is plural. To avoid API breakage, we keep it that way but
    # set it to singular here so we can determine plurality in the same way as we do
    # for `error`.
    noun = noun.replace("warnings", "warning")

    return count, noun + "s" if count != 1 else noun


class WarningReport:
    """A summary entry for stats['warnings'] (upstream's WarningReport)."""

    count_towards_summary = True

    def __init__(self, message, nodeid=None, fslocation=None):
        self.message = message
        self.nodeid = nodeid
        self.fslocation = fslocation

    def get_location(self, config=None):
        if self.nodeid:
            return self.nodeid
        if self.fslocation:
            filename, linenum = self.fslocation[:2]
            return f"{filename}:{linenum}"
        return None


class _CallableBool:
    """isatty should be a method but upstream wrongly made it a boolean;
    support both probes (upstream's compat.CallableBool)."""

    def __init__(self, value):
        self._value = bool(value)

    def __bool__(self):
        return self._value

    def __call__(self):
        return self._value


def _getreportopt(config):
    """Trimmed getreportopt: expand the -r option chars (default 'fE'
    plus warnings)."""
    try:
        reportchars = getattr(config.option, "reportchars", None) or "fE"
    except Exception:
        reportchars = "fE"
    if "w" not in reportchars:
        reportchars += "w"
    reportopts = ""
    for char in reportchars:
        if char == "a":
            reportopts = "sxXEf"
        elif char == "A":
            reportopts = "PpsxXEf"
        elif char == "N":
            reportopts = ""
        elif char not in reportopts:
            reportopts += char
    return reportopts


def _default_teststatus(report):
    """The (category, letter, word) fallback when no plugin answers
    pytest_report_teststatus (upstream's runner/skipping defaults)."""
    if hasattr(report, "wasxfail"):
        if report.skipped:
            return "xfailed", "x", "XFAIL"
        if report.passed:
            return "xpassed", "X", "XPASS"
    when = getattr(report, "when", "call")
    if when in ("setup", "teardown"):
        if report.failed:
            return "error", "E", "ERROR"
        if report.skipped:
            return "skipped", "s", "SKIPPED"
        return "", "", ""
    if report.passed:
        return "passed", ".", "PASSED"
    if report.skipped:
        return "skipped", "s", "SKIPPED"
    if report.failed:
        return "failed", "F", "FAILED"
    return "", "", ""


def _crash_message(rep):
    """The one-line crash message for short-summary suffixes: upstream reads
    longrepr.reprcrash.message; the shim's longrepr is a formatted string,
    so fall back to its first 'E ' line."""
    longrepr = getattr(rep, "longrepr", None)
    if longrepr is None:
        return None
    reprcrash = getattr(longrepr, "reprcrash", None)
    if reprcrash is not None:
        return getattr(reprcrash, "message", None)
    if isinstance(longrepr, str):
        for line in longrepr.splitlines():
            if line.startswith("E ") or line.startswith("E\t"):
                return line[1:].strip()
        for line in longrepr.splitlines():
            if line.strip():
                return line.strip()
    return None


def _get_raw_skip_reason(rep):
    """The reason suffix for verbose SKIPPED/XFAIL/XPASS words (upstream's
    _get_raw_skip_reason). xfail reports carry it on ``wasxfail`` (with a
    "reason: " prefix); skipped reports carry it as the message third of the
    longrepr tuple (with a "Skipped: " prefix)."""
    if getattr(rep, "wasxfail", None) is not None:
        reason = rep.wasxfail
        if reason.startswith("reason: "):
            reason = reason[len("reason: ") :]
        return reason
    longrepr = getattr(rep, "longrepr", None)
    if isinstance(longrepr, tuple) and len(longrepr) == 3:
        reason = longrepr[2]
    else:
        reason = _crash_message(rep) or ""
    if reason.startswith("Skipped: "):
        reason = reason[len("Skipped: ") :]
    elif reason == "Skipped":
        reason = ""
    return reason


_verbose_word_for = {
    "failed": "FAILED",
    "error": "ERROR",
    "passed": "PASSED",
    "skipped": "SKIPPED",
    "xfailed": "XFAIL",
    "xpassed": "XPASS",
}


class TerminalReporter:
    """Trimmed port of upstream's TerminalReporter: the summary-stats logic
    that upstream unit-tests directly, plus the writer/progress/summary
    surface that reporter-replacing plugins (pytest-sugar/pretty) subclass.
    Native runs never instantiate hook behavior from this class — the
    engine renders in Rust; only a registered replacement gets driven."""

    def __init__(self, config, file=None):
        self.config = config
        self.stats = {}
        self._session = None
        self._numcollected = 0
        self._main_color = None
        self._known_types = None
        self._showfspath = None
        self._progress_nodeids_reported = set()
        self._tests_ran = False
        self._already_displayed_warnings = None
        try:
            self.startpath = Path(os.getcwd())
        except OSError:
            self.startpath = Path(".")
        if file is None:
            file = sys.__stdout__ or sys.stdout
        self._tw = TerminalWriter(file)
        color = self._getoption("color", "auto")
        if color == "yes":
            self._tw.hasmarkup = True
        elif color == "no":
            self._tw.hasmarkup = False
        self._screen_width = self._tw.fullwidth
        self.currentfspath = None
        self.reportchars = _getreportopt(config)
        self.foldskipped = bool(self._getoption("fold_skipped", True))
        self.hasmarkup = self._tw.hasmarkup
        self.isatty = _CallableBool(bool(getattr(file, "isatty", lambda: False)()))
        self._show_progress_info = self._determine_show_progress_info()
        self._sessionstarttime = time.time()

    # ---- config access (defensive: upstream unit-tests hand minimal
    # config doubles to this class) --------------------------------------

    def _getoption(self, name, default=None):
        getoption = getattr(self.config, "getoption", None)
        if callable(getoption):
            try:
                value = getoption(name, default)
                return default if value is None else value
            except Exception:
                return default
        return default

    def _getini(self, name, default=None):
        getini = getattr(self.config, "getini", None)
        if callable(getini):
            try:
                value = getini(name)
                return default if value is None else value
            except Exception:
                return default
        return default

    def _determine_show_progress_info(self):
        if self._getoption("capture", "fd") == "no":
            return False
        if self._getoption("setupshow", False):
            return False
        cfg = self._getini("console_output_style", "progress")
        if cfg in {"progress", "progress-even-when-capture-no"}:
            return "progress"
        if cfg == "count":
            return "count"
        if cfg == "times":
            return "times"
        return False

    @property
    def verbosity(self):
        try:
            return int(self._getoption("verbose", 0) or 0)
        except (TypeError, ValueError):
            return 0

    @property
    def showheader(self):
        return self.verbosity >= 0

    @property
    def no_header(self):
        return bool(self._getoption("no_header", False))

    @property
    def no_summary(self):
        return bool(self._getoption("no_summary", False))

    @property
    def showfspath(self):
        if self._showfspath is None:
            return self.verbosity >= 0
        return self._showfspath

    @showfspath.setter
    def showfspath(self, value):
        self._showfspath = value

    @property
    def showlongtestinfo(self):
        return self.verbosity > 0

    @property
    def reported_progress(self):
        """The amount of items reported in the progress so far."""
        return len(self._progress_nodeids_reported)

    def hasopt(self, char):
        char = {"xfailed": "x", "skipped": "s"}.get(char, char)
        return char in self.reportchars

    # ---- writer surface --------------------------------------------------

    def write_fspath_result(self, nodeid, res, **markup):
        fspath = nodeid.split("::")[0]
        if self.currentfspath is None or fspath != self.currentfspath:
            if self.currentfspath is not None and self._show_progress_info:
                self._write_progress_information_filling_space()
            self.currentfspath = fspath
            self._tw.line()
            self._tw.write(fspath + " ")
        self._tw.write(res, flush=True, **markup)

    def write_ensure_prefix(self, prefix, extra="", **kwargs):
        if self.currentfspath != prefix:
            self._tw.line()
            self.currentfspath = prefix
            self._tw.write(prefix)
        if extra:
            self._tw.write(extra, **kwargs)
            self.currentfspath = -2

    def ensure_newline(self):
        if self.currentfspath:
            self._tw.line()
            self.currentfspath = None

    def wrap_write(self, content, *, flush=False, margin=8, line_sep="\n", **markup):
        """Wrap message with margin for progress info."""
        width_of_current_line = self._tw.width_of_current_line
        wrapped = line_sep.join(
            textwrap.wrap(
                " " * width_of_current_line + content,
                width=self._screen_width - margin,
                drop_whitespace=True,
                replace_whitespace=False,
            ),
        )
        wrapped = wrapped[width_of_current_line:]
        self._tw.write(wrapped, flush=flush, **markup)

    def write(self, content, *, flush=False, **markup):
        self._tw.write(content, flush=flush, **markup)

    def write_raw(self, content, *, flush=False):
        self._tw.write_raw(content, flush=flush)

    def flush(self):
        self._tw.flush()

    def write_line(self, line, **markup):
        if not isinstance(line, str):
            line = str(line, errors="replace")
        self.ensure_newline()
        self._tw.line(line, **markup)

    def rewrite(self, line, **markup):
        """Rewinds the terminal cursor to the beginning and writes the
        given line."""
        erase = markup.pop("erase", False)
        if erase:
            fill_count = self._tw.fullwidth - len(line) - 1
            fill = " " * fill_count
        else:
            fill = ""
        line = str(line)
        self._tw.write("\r" + line + fill, **markup)

    def write_sep(self, sep, title=None, fullwidth=None, **markup):
        self.ensure_newline()
        self._tw.sep(sep, title, fullwidth, **markup)

    def section(self, title, sep="=", **kw):
        self._tw.sep(sep, title, **kw)

    def line(self, msg, **kw):
        self._tw.line(msg, **kw)

    def _add_stats(self, category, items):
        set_main_color = category not in self.stats
        self.stats.setdefault(category, []).extend(items)
        if set_main_color:
            self._set_main_color()

    # ---- hook methods (driven via pytest._reporter when a replacement is
    # registered; subclasses inherit / override these) ---------------------

    def pytest_internalerror(self, excrepr):
        for line in str(excrepr).split("\n"):
            self.write_line("INTERNALERROR> " + line)
        return True

    def pytest_warning_recorded(self, warning_message, nodeid):
        fslocation = (
            getattr(warning_message, "filename", None),
            getattr(warning_message, "lineno", None),
        )
        self._add_stats(
            "warnings",
            [WarningReport(str(warning_message), nodeid=nodeid, fslocation=fslocation)],
        )

    def pytest_deselected(self, items):
        self._add_stats("deselected", items)

    def pytest_runtest_logstart(self, nodeid, location):
        fspath, lineno, domain = location
        # Ensure that the path is printed before the 1st test of a module
        # starts running.
        if self.showlongtestinfo:
            line = self._locationline(nodeid, fspath, lineno, domain)
            self.write_ensure_prefix(line, "")
            self.flush()
        elif self.showfspath:
            self.write_fspath_result(nodeid, "")
            self.flush()

    def _gettestkindstatus(self, report):
        """Resolve (category, letter, word) through the registered
        pytest_report_teststatus hooks, falling back to the defaults."""
        res = None
        try:
            from pytest._pluginmanager import pluginmanager

            res = pluginmanager.hook.pytest_report_teststatus(report=report, config=self.config)
        except Exception:
            res = None
        if not res:
            res = _default_teststatus(report)
        return res

    def pytest_runtest_logreport(self, report):
        self._tests_ran = True
        rep = report
        category, letter, word = self._gettestkindstatus(rep)
        if not isinstance(word, tuple):
            markup = None
        else:
            word, markup = word
        self._add_stats(category, [rep])
        if not letter and not word:
            # Probably passed setup/teardown.
            return
        if markup is None:
            was_xfail = hasattr(report, "wasxfail")
            if rep.passed and not was_xfail:
                markup = {"green": True}
            elif rep.passed and was_xfail:
                markup = {"yellow": True}
            elif rep.failed:
                markup = {"red": True}
            elif rep.skipped:
                markup = {"yellow": True}
            else:
                markup = {}
        self._progress_nodeids_reported.add(rep.nodeid)
        if self.verbosity <= 0:
            # The fspath prefix is pytest_runtest_logstart's job
            # (write_fspath_result there); here only the letter, upstream.
            self._tw.write(letter, **markup)
            if self._show_progress_info and not self._is_last_item:
                self._write_progress_information_if_past_edge()
        else:
            line = self._locationline(rep.nodeid, *rep.location)
            self.write_ensure_prefix(line, word, **markup)
            if rep.skipped or hasattr(report, "wasxfail"):
                reason = _get_raw_skip_reason(rep)
                if reason:
                    self.wrap_write(f" ({reason})")
            if self._show_progress_info:
                self._write_progress_information_filling_space()
        self.flush()

    def pytest_runtest_logfinish(self, nodeid):
        if self.verbosity <= 0 and self._show_progress_info and self._is_last_item:
            self._write_progress_information_filling_space()

    @property
    def _is_last_item(self):
        if self._session is None:
            return False
        return self.reported_progress == self._session.testscollected

    def _get_progress_information_message(self):
        collected = self._session.testscollected if self._session else 0
        if self._show_progress_info == "count":
            if collected:
                progress = self.reported_progress
                counter_format = f"{{:{len(str(collected))}d}}"
                format_string = f" [{counter_format}/{{}}]"
                return format_string.format(progress, collected)
            return f" [ {collected} / {collected} ]"
        if collected:
            return f" [{self.reported_progress * 100 // collected:3d}%]"
        return " [100%]"

    def _write_progress_information_if_past_edge(self):
        w = self._width_of_current_line
        if self._show_progress_info == "count":
            num_tests = self._session.testscollected if self._session else 0
            progress_length = len(f" [{num_tests}/{num_tests}]")
        else:
            progress_length = len(" [100%]")
        past_edge = w + progress_length + 1 >= self._screen_width
        if past_edge:
            main_color, _ = self._get_main_color()
            msg = self._get_progress_information_message()
            self._tw.write(msg + "\n", **{main_color: True})

    def _write_progress_information_filling_space(self):
        color, _ = self._get_main_color()
        msg = self._get_progress_information_message()
        w = self._width_of_current_line
        fill = self._tw.fullwidth - w - 1
        self.write(msg.rjust(fill), flush=True, **{color: True})

    @property
    def _width_of_current_line(self):
        """Return the width of the current line."""
        return self._tw.width_of_current_line

    def pytest_collectreport(self, report):
        if report.failed:
            self._add_stats("error", [report])
        elif report.skipped:
            self._add_stats("skipped", [report])
        self._numcollected += len(getattr(report, "result", ()) or ())

    def report_collect(self, final=False):
        if self.verbosity < 0:
            return
        errors = len(self.stats.get("error", []))
        skipped = len(self.stats.get("skipped", []))
        deselected = len(self.stats.get("deselected", []))
        selected = self._numcollected - deselected
        line = "collected " if final else "collecting "
        line += str(self._numcollected) + " item" + ("" if self._numcollected == 1 else "s")
        if errors:
            line += f" / {errors} error{'s' if errors != 1 else ''}"
        if deselected:
            line += f" / {deselected} deselected"
        if skipped:
            line += f" / {skipped} skipped"
        if self._numcollected > selected:
            line += f" / {selected} selected"
        if self.isatty():
            self.rewrite(line, bold=True, erase=True)
            if final:
                self.write("\n")
        else:
            # Piped -v shows upstream's collection-start "collecting ... "
            # prefix on the same line (pytest_collection writes it unflushed).
            # Skip the prefix when errors occurred: instafail's
            # pytest_collectreport already printed the error traceback,
            # breaking the visual continuity.
            if final and self.verbosity >= 1 and not self.stats.get("error"):
                self.write("collecting ... ", bold=True)
            self.write_line(line)

    def pytest_sessionstart(self, session):
        self._session = session
        self._sessionstarttime = time.time()
        if not self.showheader:
            return
        self.write_sep("=", "test session starts", bold=True)
        if not self.no_header:
            verinfo = platform.python_version()
            import pytest

            # Match the native engine header ("pytest-rs-<crate version>", no
            # pluggy): a replacement reporter's header must equal the native
            # one — pytest-bdd's gherkin reporter test compares them line by line.
            rs_version = getattr(pytest, "_rs_version", pytest.__version__)
            msg = f"platform {sys.platform} -- Python {verinfo}, pytest-rs-{rs_version}"
            if self.verbosity > 0:
                msg += " -- " + str(sys.executable)
            self.write_line(msg)
            try:
                from pytest._pluginmanager import pluginmanager

                lines = pluginmanager.hook.pytest_report_header(
                    config=self.config, start_path=self.startpath
                )
            except Exception:
                lines = []
            self._write_report_lines_from_hooks(lines or [])

    def _write_report_lines_from_hooks(self, lines):
        for line_or_lines in reversed(lines):
            if isinstance(line_or_lines, str):
                self.write_line(line_or_lines)
            else:
                for line in line_or_lines:
                    self.write_line(line)

    def pytest_collection_finish(self, session):
        self.report_collect(True)
        # Upstream: plugins contribute trailing collection lines (e.g.
        # pytest-run-parallel's "Collected N items to run in parallel").
        try:
            lines = self.config.hook.pytest_report_collectionfinish(
                config=self.config,
                start_path=self.startpath,
                items=getattr(session, "items", None) or [],
            )
            self._write_report_lines_from_hooks(lines or [])
        except Exception:
            pass
        # --collect-only: the engine prints the tree natively; open the
        # blank line above it like upstream's _printcollecteditems flow
        # (suppressed under -q, like upstream's verbose > -1 check).
        if (
            self._getoption("collectonly", False)
            and getattr(session, "items", None)
            and self.verbosity > -1
        ):
            self._tw.line("")

    def pytest_sessionfinish(self, session, exitstatus):
        self._tw.line("")
        self.summary_stats()

    def _locationline(self, nodeid, fspath, lineno, domain):
        def mkrel(nodeid):
            line = nodeid
            if domain and line.endswith(domain):
                line = line[: -len(domain)]
                values = domain.split("[")
                values[0] = values[0].replace(".", "::")  # don't replace '.' in params
                line += "[".join(values)
            return line

        # fspath comes from testid which has a "/"-normalized path.
        if fspath:
            res = mkrel(nodeid)
            if self.verbosity >= 2 and nodeid.split("::")[0] != fspath.replace("\\", "/"):
                res += " <- " + fspath
        else:
            res = "[location]"
        return res + " "

    def _getfailureheadline(self, rep):
        head_line = getattr(rep, "head_line", None)
        if head_line:
            return head_line
        return "test session"  # XXX?

    def _getcrashline(self, rep):
        try:
            return str(rep.longrepr.reprcrash)
        except AttributeError:
            try:
                return str(rep.longrepr)[:50]
            except AttributeError:
                return ""

    #
    # Summaries for sessionfinish.
    #
    def getreports(self, name):
        return [x for x in self.stats.get(name, ()) if not hasattr(x, "_pdbshown")]

    def summary_warnings(self):
        if self.hasopt("w"):
            all_warnings = self.stats.get("warnings")
            if not all_warnings:
                return

            final = self._already_displayed_warnings is not None
            if final:
                warning_reports = all_warnings[self._already_displayed_warnings :]
            else:
                warning_reports = all_warnings
            self._already_displayed_warnings = len(warning_reports)
            if not warning_reports:
                return

            reports_grouped_by_message = {}
            for wr in warning_reports:
                reports_grouped_by_message.setdefault(wr.message, []).append(wr)

            title = "warnings summary (final)" if final else "warnings summary"
            self.write_sep("=", title, yellow=True, bold=False)
            for message, message_reports in reports_grouped_by_message.items():
                locations = []
                for w in message_reports:
                    location = w.get_location(self.config)
                    if location:
                        locations.append(location)
                if locations:
                    self._tw.line("\n".join(map(str, locations)))
                    lines = message.splitlines()
                    indented = "\n".join("  " + x for x in lines)
                    message = indented.rstrip()
                else:
                    message = message.rstrip()
                self._tw.line(message)
                self._tw.line()
            self._tw.line("-- Docs: https://docs.pytest.org/en/stable/how-to/capture-warnings.html")

    def summary_passes(self):
        self.summary_passes_combined("passed", "PASSES", "P")

    def summary_xpasses(self):
        self.summary_passes_combined("xpassed", "XPASSES", "X")

    def summary_passes_combined(self, which_reports, sep_title, needed_opt):
        if self._getoption("tbstyle", "auto") != "no":
            if self.hasopt(needed_opt):
                reports = self.getreports(which_reports)
                if not reports:
                    return
                self.write_sep("=", sep_title)
                for rep in reports:
                    if rep.sections:
                        msg = self._getfailureheadline(rep)
                        self.write_sep("_", msg, green=True, bold=True)
                        self._outrep_summary(rep)

    def summary_failures(self):
        style = self._getoption("tbstyle", "auto")
        self.summary_failures_combined("failed", "FAILURES", style=style)

    def summary_xfailures(self):
        show_tb = self._getoption("xfail_tb", False)
        style = self._getoption("tbstyle", "auto") if show_tb else "no"
        self.summary_failures_combined("xfailed", "XFAILURES", style=style)

    def summary_failures_combined(self, which_reports, sep_title, *, style, needed_opt=None):
        if style != "no":
            if not needed_opt or self.hasopt(needed_opt):
                reports = self.getreports(which_reports)
                if not reports:
                    return
                self.write_sep("=", sep_title)
                if style == "line":
                    for rep in reports:
                        line = self._getcrashline(rep)
                        self.write_line(line)
                else:
                    for rep in reports:
                        msg = self._getfailureheadline(rep)
                        self.write_sep("_", msg, red=True, bold=True)
                        self._outrep_summary(rep)

    def summary_errors(self):
        if self._getoption("tbstyle", "auto") != "no":
            reports = self.getreports("error")
            if not reports:
                return
            self.write_sep("=", "ERRORS")
            for rep in self.stats["error"]:
                msg = self._getfailureheadline(rep)
                when = getattr(rep, "when", "collect")
                if when == "collect":
                    msg = "ERROR collecting " + msg
                else:
                    msg = f"ERROR at {when} of {msg}"
                self.write_sep("_", msg, red=True, bold=True)
                self._outrep_summary(rep)

    def _outrep_summary(self, rep):
        rep.toterminal(self._tw)
        showcapture = self._getoption("showcapture", "all")
        if showcapture == "no":
            return
        for secname, content in getattr(rep, "sections", ()) or ():
            if showcapture != "all" and showcapture not in secname:
                continue
            self._tw.sep("-", secname)
            if content[-1:] == "\n":
                content = content[:-1]
            self._tw.line(content)

    def summary_stats(self):
        if self.verbosity < -1:
            return

        session_duration = time.time() - self._sessionstarttime
        (parts, main_color) = self.build_summary_stats_line()
        line_parts = []

        display_sep = self.verbosity >= 0
        if display_sep:
            fullwidth = self._tw.fullwidth
        for text, markup in parts:
            with_markup = self._tw.markup(text, **markup)
            if display_sep:
                fullwidth += len(with_markup) - len(text)
            line_parts.append(with_markup)
        msg = ", ".join(line_parts)

        main_markup = {main_color: True}
        duration = f" in {format_session_duration(session_duration)}"
        duration_with_markup = self._tw.markup(duration, **main_markup)
        if display_sep:
            fullwidth += len(duration_with_markup) - len(duration)
        msg += duration_with_markup

        if display_sep:
            markup_for_end_sep = self._tw.markup("", **main_markup)
            if markup_for_end_sep.endswith("\x1b[0m"):
                markup_for_end_sep = markup_for_end_sep[:-4]
            fullwidth += len(markup_for_end_sep)
            msg += markup_for_end_sep

        if display_sep:
            self.write_sep("=", msg, fullwidth=fullwidth, **main_markup)
        else:
            self.write_line(msg, **main_markup)

    def short_test_summary(self):
        if not self.reportchars:
            return

        def show_simple(lines, *, stat):
            failed = self.stats.get(stat, [])
            if not failed:
                return
            color = _color_for_type.get(stat, _color_for_type_default)
            word = _verbose_word_for.get(stat, stat.upper())
            for rep in failed:
                line = f"{self._tw.markup(word, **{color: True})} {rep.nodeid}"
                msg = _crash_message(rep)
                if msg:
                    max_len = self._tw.fullwidth - len(word) - len(rep.nodeid) - 4
                    if max_len > 5:
                        msg = msg.replace("\n", " ")
                        if len(msg) > max_len:
                            msg = msg[: max_len - 3] + "..."
                        line += f" - {msg}"
                lines.append(line)

        def show_xfailed(lines):
            for rep in self.stats.get("xfailed", []):
                markup_word = self._tw.markup("XFAIL", yellow=True)
                line = f"{markup_word} {rep.nodeid}"
                reason = getattr(rep, "wasxfail", None)
                if reason:
                    line += " - " + str(reason)
                lines.append(line)

        def show_xpassed(lines):
            for rep in self.stats.get("xpassed", []):
                markup_word = self._tw.markup("XPASS", yellow=True)
                line = f"{markup_word} {rep.nodeid}"
                reason = getattr(rep, "wasxfail", None)
                if reason:
                    line += " - " + str(reason)
                lines.append(line)

        def show_skipped(lines):
            skipped = self.stats.get("skipped", [])
            if not skipped:
                return
            markup_word = self._tw.markup("SKIPPED", yellow=True)
            if self.foldskipped:
                by_reason = {}
                for rep in skipped:
                    reason = _crash_message(rep) or ""
                    if reason.startswith("Skipped: "):
                        reason = reason[len("Skipped: ") :]
                    by_reason.setdefault(reason, []).append(rep)
                for reason, reps in by_reason.items():
                    lines.append(f"{markup_word} [{len(reps)}] {reason}")
            else:
                for rep in skipped:
                    reason = _crash_message(rep) or ""
                    lines.append(f"{markup_word} {rep.nodeid} - {reason}")

        reportchar_actions = {
            "x": show_xfailed,
            "X": show_xpassed,
            "f": partial(show_simple, stat="failed"),
            "s": show_skipped,
            "p": partial(show_simple, stat="passed"),
            "E": partial(show_simple, stat="error"),
        }

        lines = []
        for char in self.reportchars:
            action = reportchar_actions.get(char)
            if action:  # skipping e.g. "P" (passed with output) here.
                action(lines)

        if lines:
            self.write_sep("=", "short test summary info", cyan=True, bold=True)
            for line in lines:
                self.write_line(line)

    # ---- summary-stats logic (upstream unit-tested) ----------------------

    def _get_main_color(self):
        if self._main_color is None or self._known_types is None or self._is_last_item:
            self._set_main_color()
            assert self._main_color
            assert self._known_types
        return self._main_color, self._known_types

    def _determine_main_color(self, unknown_type_seen):
        stats = self.stats
        if "failed" in stats or "error" in stats:
            main_color = "red"
        elif "warnings" in stats or "xpassed" in stats or unknown_type_seen:
            main_color = "yellow"
        elif "passed" in stats or not self._is_last_item:
            main_color = "green"
        else:
            main_color = "yellow"
        return main_color

    def _set_main_color(self):
        unknown_types = []
        for found_type in self.stats:
            if found_type:  # setup/teardown reports have an empty key, ignore them
                if found_type not in KNOWN_TYPES and found_type not in unknown_types:
                    unknown_types.append(found_type)
        self._known_types = list(KNOWN_TYPES) + unknown_types
        self._main_color = self._determine_main_color(bool(unknown_types))

    def build_summary_stats_line(self):
        """Build the (text, markup) parts of the final summary line plus its
        main color, like upstream."""
        if self.config.getoption("collectonly"):
            return self._build_collect_only_summary_stats_line()
        else:
            return self._build_normal_summary_stats_line()

    def _get_reports_to_display(self, key):
        """Get test/collection reports for the given status key, such as `passed` or `error`."""
        reports = self.stats.get(key, [])
        return [x for x in reports if getattr(x, "count_towards_summary", True)]

    def _build_normal_summary_stats_line(self):
        main_color, known_types = self._get_main_color()
        parts = []

        for key in known_types:
            reports = self._get_reports_to_display(key)
            if reports:
                count = len(reports)
                color = _color_for_type.get(key, _color_for_type_default)
                markup = {color: True, "bold": color == main_color}
                parts.append(("%d %s" % pluralize(count, key), markup))  # noqa: UP031

        if not parts:
            parts = [("no tests ran", {_color_for_type_default: True})]

        return parts, main_color

    def _build_collect_only_summary_stats_line(self):
        deselected = len(self._get_reports_to_display("deselected"))
        errors = len(self._get_reports_to_display("error"))

        if self._numcollected == 0:
            parts = [("no tests collected", {"yellow": True})]
            main_color = "yellow"
        elif deselected == 0:
            main_color = "green"
            collected_output = "%d %s collected" % pluralize(self._numcollected, "test")  # noqa: UP031
            parts = [(collected_output, {main_color: True})]
        else:
            all_tests_were_deselected = self._numcollected == deselected
            if all_tests_were_deselected:
                main_color = "yellow"
                collected_output = f"no tests collected ({deselected} deselected)"
            else:
                main_color = "green"
                selected = self._numcollected - deselected
                collected_output = (
                    f"{selected}/{self._numcollected} tests collected ({deselected} deselected)"
                )

            parts = [(collected_output, {main_color: True})]

        if errors:
            main_color = _color_for_type["error"]
            parts += [("%d %s" % pluralize(errors, "error"), {main_color: True})]  # noqa: UP031

        return parts, main_color


_REPORTCHARS_DEFAULT = "fE"


def getreportopt(config) -> str:
    """The effective -r report chars, with aliases expanded (pytest's
    getreportopt): a/A expand groups, N clears, F/S are old lowercase
    aliases, and 'w' (warnings) is forced on unless --disable-warnings."""
    reportchars: str = config.option.reportchars

    old_aliases = {"F", "S"}
    reportopts = ""
    for char in reportchars:
        if char in old_aliases:
            char = char.lower()
        if char == "a":
            reportopts = "sxXEf"
        elif char == "A":
            reportopts = "PpsxXEf"
        elif char == "N":
            reportopts = ""
        elif char not in reportopts:
            reportopts += char

    disable_warnings = getattr(config.option, "disable_warnings", False)
    if not disable_warnings and "w" not in reportopts:
        reportopts = "w" + reportopts
    elif disable_warnings and "w" in reportopts:
        reportopts = reportopts.replace("w", "")

    return reportopts


def _format_trimmed(format: str, msg: str, available_width: int):
    """Format msg into format, ellipsizing it if it doesn't fit in
    available_width (pytest's helper). Returns None if even the ellipsis
    can't fit."""
    # Only use the first line.
    i = msg.find("\n")
    if i != -1:
        msg = msg[:i]

    ellipsis = "..."
    format_width = wcswidth(format.format(""))
    if format_width + len(ellipsis) > available_width:
        return None

    if format_width + wcswidth(msg) > available_width:
        available_width -= len(ellipsis)
        msg = msg[:available_width]
        while format_width + wcswidth(msg) > available_width:
            msg = msg[:-1]
        msg += ellipsis

    return format.format(msg)


def format_verbose_reason(prefix_width, reason, verbosity, fullwidth):
    """The reason suffix for a verbose skip/xfail/xpass line: truncated to fit
    (verbosity < 2) or wrapped across lines (verbosity >= 2). An empty reason
    returns ''. Mirrors TerminalReporter.pytest_runtest_logreport's skip/xfail
    reason handling (wrap_write uses screen_width - margin, margin=8)."""
    if not reason:
        return ""
    if verbosity < 2:
        available = fullwidth - prefix_width - len(" [100%]") - 1
        return _format_trimmed(" ({})", reason, available) or ""
    content = f" ({reason})"
    wrapped = "\n".join(
        textwrap.wrap(
            " " * prefix_width + content,
            width=fullwidth - 8,
            drop_whitespace=True,
            replace_whitespace=False,
        )
    )
    return wrapped[prefix_width:]


def _get_node_id_with_markup(tw, config, rep):
    nodeid = config.cwd_relative_nodeid(rep.nodeid)
    path, *parts = nodeid.split("::")
    if parts:
        parts_markup = tw.markup("::".join(parts), bold=True)
        return path + "::" + parts_markup
    else:
        return path


def _get_line_with_reprcrash_message(config, rep, tw, word_markup):
    """Summary line for a report, trying to append the reprcrash message
    (trimmed to the terminal width unless on CI / -vv)."""
    verbose_word, verbose_markup = rep._get_verbose_word_with_markup(config, word_markup)
    word = tw.markup(verbose_word, **verbose_markup)
    node = _get_node_id_with_markup(tw, config, rep)

    line = f"{word} {node}"
    line_width = wcswidth(line)

    try:
        if isinstance(rep.longrepr, str):
            msg = rep.longrepr
        else:
            msg = rep.longrepr.reprcrash.message
    except AttributeError:
        pass
    else:
        if (running_on_ci() or getattr(config.option, "verbose", 0) >= 2) and not getattr(
            config.option, "force_short_summary", False
        ):
            msg = f" - {msg}"
        else:
            available_width = tw.fullwidth - line_width
            msg = _format_trimmed(" - {}", msg, available_width)
        if msg is not None:
            line += msg

    return line


def _bestrelpath(directory, dest):
    """A relative path from directory to dest, or str(dest) when none exists
    (mixed absolute/relative or different drives) — pytest's bestrelpath."""
    if dest == directory:
        return os.curdir
    try:
        return os.path.relpath(dest, directory)
    except ValueError:
        return str(dest)


def _folded_skips(startpath, skipped):
    """Group skip reports by (fspath, lineno, reason) so the short summary
    can fold duplicates into "SKIPPED [N] fspath:lineno: reason"."""
    d: dict = {}
    for event in skipped:
        assert event.longrepr is not None
        assert isinstance(event.longrepr, tuple), (event, event.longrepr)
        assert len(event.longrepr) == 3, (event, event.longrepr)
        fspath, lineno, reason = event.longrepr
        fspath = _bestrelpath(startpath, Path(fspath))
        keywords = getattr(event, "keywords", {})
        if event.when == "setup" and "skip" in keywords and "pytestmark" not in keywords:
            key = (fspath, None, reason)
        else:
            key = (fspath, lineno, reason)
        d.setdefault(key, []).append(event)
    values = []
    for key, events in d.items():
        values.append((len(events), *key))
    return values


class TerminalProgressPlugin:
    """Terminal progress reporting via OSC 9;4 ANSI sequences (upstream port).
    Emits sequences indicating test progress to supporting terminal tabs."""

    def __init__(self, tr):
        self._tr = tr
        self._session = None
        self._has_failures = False

    def _emit_progress(self, state, progress=None):
        """Emit the OSC 9;4 sequence for a progress state (0-100)."""
        assert progress is None or 0 <= progress <= 100
        if state == "remove":
            sequence = "\x1b]9;4;0;\x1b\\"
        elif state == "normal":
            assert progress is not None
            sequence = f"\x1b]9;4;1;{progress}\x1b\\"
        elif state == "error":
            sequence = (
                f"\x1b]9;4;2;{progress}\x1b\\" if progress is not None else "\x1b]9;4;2;\x1b\\"
            )
        elif state == "indeterminate":
            sequence = "\x1b]9;4;3;\x1b\\"
        elif state == "paused":
            sequence = (
                f"\x1b]9;4;4;{progress}\x1b\\" if progress is not None else "\x1b]9;4;4;\x1b\\"
            )
        else:
            return
        self._tr.write_raw(sequence, flush=True)

    def pytest_sessionstart(self, session):
        self._session = session
        self._emit_progress("indeterminate")

    def pytest_collection_finish(self):
        assert self._session is not None
        if self._session.testscollected > 0:
            self._emit_progress("normal", 0)

    def pytest_runtest_logreport(self, report):
        if report.failed:
            self._has_failures = True
        if report.when != "call":
            return
        assert self._session is not None
        collected = self._session.testscollected
        if collected > 0:
            reported = self._tr.reported_progress
            progress = min(reported * 100 // collected, 100)
            self._emit_progress("error" if self._has_failures else "normal", progress)

    def pytest_sessionfinish(self):
        self._emit_progress("remove")


from _pytest._stub import __getattr__  # noqa: E402, F401
