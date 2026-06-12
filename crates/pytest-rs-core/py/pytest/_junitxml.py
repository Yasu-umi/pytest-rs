"""--junitxml report writer: a port of pytest's _pytest/junitxml.py.

LogXML/_NodeReporter/bin_xml_escape are kept faithful to upstream (its own
suite unit-tests them); _pytest.junitxml re-exports them. The Rust engine
creates the LogXML at session start (configure()), streams every TestReport
through log_report() at session end, then finish() writes the file. The
record_property/record_xml_attribute/record_testsuite_property fixtures
live here too.
"""

import datetime
import os
import platform
import re
import time
import xml.etree.ElementTree as ET

from pytest._fixtures import fixture
from pytest._warning_types import PytestExperimentalApiWarning, PytestWarning


def bin_xml_escape(arg):
    r"""Visually escape invalid XML characters.

    For example, transforms 'hello\aworld\b' into 'hello#x07world#x08'.
    Note that the #xABs are *not* XML escapes - missing the ampersand &#xAB.
    The idea is to escape visually for the user rather than for XML itself.
    """

    def repl(matchobj):
        i = ord(matchobj.group())
        if i <= 0xFF:
            return f"#x{i:02X}"
        else:
            return f"#x{i:04X}"

    # The spec range of valid chars is:
    # Char ::= #x9 | #xA | #xD | [#x20-#xD7FF] | [#xE000-#xFFFD] | [#x10000-#x10FFFF]
    # For an unknown(?) reason, we disallow #x7F (DEL) as well.
    illegal_xml_re = (
        "[^\\u0009\\u000a\\u000d\\u0020-\\u007e\\u0080-\\ud7ff"
        "\\ue000-\\ufffd\\U00010000-\\U0010ffff]"
    )
    return re.sub(illegal_xml_re, repl, str(arg))


def merge_family(left, right):
    result = {}
    for kl, vl in left.items():
        for kr, vr in right.items():
            if not isinstance(vl, list):
                raise TypeError(type(vl))
            result[kl] = vl + vr
    left.update(result)


families = {
    "_base": {"testcase": ["classname", "name"]},
    "_base_legacy": {"testcase": ["file", "line", "url"]},
}
# xUnit 1.x inherits legacy attributes.
families["xunit1"] = families["_base"].copy()
merge_family(families["xunit1"], families["_base_legacy"])

# xUnit 2.x uses strict base attributes.
families["xunit2"] = families["_base"]


def mangle_test_address(address):
    path, possible_open_bracket, params = address.partition("[")
    names = path.split("::")
    # Convert file path to dotted path.
    names[0] = names[0].replace("/", ".")
    names[0] = re.sub(r"\.py$", "", names[0])
    # Put any params back.
    names[-1] += possible_open_bracket + params
    return names


class _NodeReporter:
    def __init__(self, nodeid, xml):
        self.id = nodeid
        self.xml = xml
        self.add_stats = self.xml.add_stats
        self.family = self.xml.family
        self.duration = 0.0
        self.properties = []
        self.nodes = []
        self.attrs = {}

    def append(self, node):
        self.xml.add_stats(node.tag)
        self.nodes.append(node)

    def add_property(self, name, value):
        self.properties.append((str(name), bin_xml_escape(value)))

    def add_attribute(self, name, value):
        self.attrs[str(name)] = bin_xml_escape(value)

    def make_properties_node(self):
        """Return a Junit node containing custom properties, if any."""
        if self.properties:
            properties = ET.Element("properties")
            for name, value in self.properties:
                properties.append(ET.Element("property", name=name, value=value))
            return properties
        return None

    def record_testreport(self, testreport):
        names = mangle_test_address(testreport.nodeid)
        existing_attrs = self.attrs
        classnames = names[:-1]
        if self.xml.prefix:
            classnames.insert(0, self.xml.prefix)
        attrs = {
            "classname": ".".join(classnames),
            "name": bin_xml_escape(names[-1]),
            "file": testreport.location[0],
        }
        if testreport.location[1] is not None:
            attrs["line"] = str(testreport.location[1])
        if hasattr(testreport, "url"):
            attrs["url"] = testreport.url
        self.attrs = attrs
        self.attrs.update(existing_attrs)  # Restore any user-defined attributes.

        # Preserve legacy testcase behavior.
        if self.family == "xunit1":
            return

        # Filter out attributes not permitted by this test family.
        # Including custom attributes because they are not valid here.
        temp_attrs = {}
        for key in self.attrs:
            if key in families[self.family]["testcase"]:
                temp_attrs[key] = self.attrs[key]
        self.attrs = temp_attrs

    def to_xml(self):
        testcase = ET.Element("testcase", self.attrs, time=f"{self.duration:.3f}")
        properties = self.make_properties_node()
        if properties is not None:
            testcase.append(properties)
        testcase.extend(self.nodes)
        return testcase

    def _add_simple(self, tag, message, data=None):
        node = ET.Element(tag, message=message)
        node.text = bin_xml_escape(data)
        self.append(node)

    def write_captured_output(self, report):
        if not self.xml.log_passing_tests and report.passed:
            return

        content_out = report.capstdout
        content_log = report.caplog
        content_err = report.capstderr
        if self.xml.logging == "no":
            return
        content_all = ""
        if self.xml.logging in ["log", "all"]:
            content_all = self._prepare_content(content_log, " Captured Log ")
        if self.xml.logging in ["system-out", "out-err", "all"]:
            content_all += self._prepare_content(content_out, " Captured Out ")
            self._write_content(report, content_all, "system-out")
            content_all = ""
        if self.xml.logging in ["system-err", "out-err", "all"]:
            content_all += self._prepare_content(content_err, " Captured Err ")
            self._write_content(report, content_all, "system-err")
            content_all = ""
        if content_all:
            self._write_content(report, content_all, "system-out")

    def _prepare_content(self, content, header):
        return "\n".join([header.center(80, "-"), content, ""])

    def _write_content(self, report, content, jheader):
        tag = ET.Element(jheader)
        tag.text = bin_xml_escape(content)
        self.append(tag)

    def append_pass(self, report):
        self.add_stats("passed")

    def append_failure(self, report):
        if hasattr(report, "wasxfail"):
            self._add_simple("skipped", "xfail-marked test passes unexpectedly")
        else:
            assert report.longrepr is not None
            reprcrash = getattr(report.longrepr, "reprcrash", None)
            if reprcrash is not None:
                message = reprcrash.message
            else:
                message = str(report.longrepr)
            message = bin_xml_escape(message)
            self._add_simple("failure", message, str(report.longrepr))

    def append_collect_error(self, report):
        assert report.longrepr is not None
        self._add_simple("error", "collection failure", str(report.longrepr))

    def append_collect_skipped(self, report):
        self._add_simple("skipped", "collection skipped", str(report.longrepr))

    def append_error(self, report):
        assert report.longrepr is not None
        reprcrash = getattr(report.longrepr, "reprcrash", None)
        if reprcrash is not None:
            reason = reprcrash.message
        else:
            reason = str(report.longrepr)

        if report.when == "teardown":
            msg = f'failed on teardown with "{reason}"'
        else:
            msg = f'failed on setup with "{reason}"'
        self._add_simple("error", bin_xml_escape(msg), str(report.longrepr))

    def append_skipped(self, report):
        if hasattr(report, "wasxfail"):
            xfailreason = report.wasxfail
            if xfailreason.startswith("reason: "):
                xfailreason = xfailreason[8:]
            xfailreason = bin_xml_escape(xfailreason)
            skipped = ET.Element("skipped", type="pytest.xfail", message=xfailreason)
            self.append(skipped)
        else:
            assert isinstance(report.longrepr, tuple)
            filename, lineno, skipreason = report.longrepr
            if skipreason.startswith("Skipped: "):
                skipreason = skipreason[9:]
            details = f"{filename}:{lineno}: {skipreason}"

            skipped = ET.Element("skipped", type="pytest.skip", message=bin_xml_escape(skipreason))
            skipped.text = bin_xml_escape(details)
            self.append(skipped)
            self.write_captured_output(report)

    def finalize(self):
        data = self.to_xml()
        self.__dict__.clear()
        self.to_xml = lambda: data


def _check_record_param_type(param, v):
    """Used by record_testsuite_property to check that the given parameter
    name is of the proper type."""
    __tracebackhide__ = True
    if not isinstance(v, str):
        msg = "{param} parameter needs to be a string, but {g} given"
        raise TypeError(msg.format(param=param, g=type(v).__name__))


class LogXML:
    def __init__(
        self,
        logfile,
        prefix=None,
        suite_name="pytest",
        logging="no",
        report_duration="total",
        family="xunit1",
        log_passing_tests=True,
    ):
        logfile = os.path.expanduser(os.path.expandvars(logfile))
        self.logfile = os.path.normpath(os.path.abspath(logfile))
        self.prefix = prefix
        self.suite_name = suite_name
        self.logging = logging
        self.log_passing_tests = log_passing_tests
        self.report_duration = report_duration
        self.family = family
        self.stats = dict.fromkeys(["error", "passed", "failure", "skipped"], 0)
        self.node_reporters = {}
        self.node_reporters_ordered = []
        self.global_properties = []

        # List of reports that failed on call but teardown is pending.
        self.open_reports = []
        self.cnt_double_fail_tests = 0

        # Replaces convenience family with real family.
        if self.family == "legacy":
            self.family = "xunit1"

    def finalize(self, report):
        nodeid = getattr(report, "nodeid", report)
        workernode = getattr(report, "node", None)
        reporter = self.node_reporters.pop((nodeid, workernode))

        for propname, propvalue in report.user_properties:
            reporter.add_property(propname, str(propvalue))

        if reporter is not None:
            reporter.finalize()

    def node_reporter(self, report):
        nodeid = getattr(report, "nodeid", report)
        workernode = getattr(report, "node", None)

        key = nodeid, workernode

        if key in self.node_reporters:
            return self.node_reporters[key]

        reporter = _NodeReporter(nodeid, self)

        self.node_reporters[key] = reporter
        self.node_reporters_ordered.append(reporter)

        return reporter

    def add_stats(self, key):
        if key in self.stats:
            self.stats[key] += 1

    def _opentestcase(self, report):
        reporter = self.node_reporter(report)
        reporter.record_testreport(report)
        return reporter

    def pytest_runtest_logreport(self, report):
        """Handle a setup/call/teardown report, generating the appropriate
        XML tags as necessary."""
        close_report = None
        if report.passed:
            if report.when == "call":  # ignore setup/teardown
                reporter = self._opentestcase(report)
                reporter.append_pass(report)
        elif report.failed:
            if report.when == "teardown":
                close_report = next(
                    (rep for rep in self.open_reports if rep.nodeid == report.nodeid),
                    None,
                )
                if close_report:
                    # A failure in call plus an error in teardown needs two
                    # testcases to follow the junit schema.
                    self.finalize(close_report)
                    self.cnt_double_fail_tests += 1
            reporter = self._opentestcase(report)
            if report.when == "call":
                reporter.append_failure(report)
                self.open_reports.append(report)
                if not self.log_passing_tests:
                    reporter.write_captured_output(report)
            else:
                reporter.append_error(report)
        elif report.skipped:
            reporter = self._opentestcase(report)
            reporter.append_skipped(report)
        self.update_testcase_duration(report)
        if report.when == "teardown":
            reporter = self._opentestcase(report)
            reporter.write_captured_output(report)

            self.finalize(report)
            close_report = next(
                (rep for rep in self.open_reports if rep.nodeid == report.nodeid),
                None,
            )
            if close_report:
                self.open_reports.remove(close_report)

    def update_testcase_duration(self, report):
        """Accumulate total duration for nodeid from given report and update
        the Junit.testcase with the new total if already created."""
        if self.report_duration in {"total", report.when}:
            reporter = self.node_reporter(report)
            reporter.duration += getattr(report, "duration", 0.0)

    def pytest_collectreport(self, report):
        if not report.passed:
            reporter = self._opentestcase(report)
            if report.failed:
                reporter.append_collect_error(report)
            else:
                reporter.append_collect_skipped(report)

    def pytest_internalerror(self, excrepr):
        reporter = self.node_reporter("internal")
        reporter.attrs.update(classname="pytest", name="internal")
        reporter._add_simple("error", "internal error", str(excrepr))

    def pytest_sessionstart(self):
        self.suite_start = datetime.datetime.now(datetime.UTC)
        self._suite_start_monotonic = time.monotonic()

    def pytest_sessionfinish(self):
        dirname = os.path.dirname(os.path.abspath(self.logfile))
        # exist_ok avoids filesystem race conditions between checking path
        # existence and requesting creation.
        os.makedirs(dirname, exist_ok=True)

        with open(self.logfile, "w", encoding="utf-8") as logfile:
            duration = time.monotonic() - self._suite_start_monotonic

            numtests = (
                self.stats["passed"]
                + self.stats["failure"]
                + self.stats["skipped"]
                + self.stats["error"]
                - self.cnt_double_fail_tests
            )
            logfile.write('<?xml version="1.0" encoding="utf-8"?>')

            suite_node = ET.Element(
                "testsuite",
                name=self.suite_name,
                errors=str(self.stats["error"]),
                failures=str(self.stats["failure"]),
                skipped=str(self.stats["skipped"]),
                tests=str(numtests),
                time=f"{duration:.3f}",
                timestamp=self.suite_start.astimezone().isoformat(),
                hostname=platform.node(),
            )
            global_properties = self._get_global_properties_node()
            if global_properties is not None:
                suite_node.append(global_properties)
            for node_reporter in self.node_reporters_ordered:
                suite_node.append(node_reporter.to_xml())
            testsuites = ET.Element("testsuites")
            testsuites.set("name", "pytest tests")
            testsuites.append(suite_node)
            logfile.write(ET.tostring(testsuites, encoding="unicode"))

    def add_global_property(self, name, value):
        __tracebackhide__ = True
        _check_record_param_type("name", name)
        self.global_properties.append((name, bin_xml_escape(value)))

    def _get_global_properties_node(self):
        """Return a Junit node containing custom properties, if any."""
        if self.global_properties:
            properties = ET.Element("properties")
            for name, value in self.global_properties:
                properties.append(ET.Element("property", name=name, value=value))
            return properties
        return None


class _CrashLocation:
    def __init__(self, message):
        self.message = message


class _LongReprText:
    """A str-like longrepr with pytest's .reprcrash.message — the short
    "ValueError: 42" essence junit uses for failure/error message
    attributes, recovered from the formatted traceback's E lines."""

    def __init__(self, text):
        self._text = text
        e_lines = [
            line[1:].lstrip() for line in text.splitlines() if line.startswith("E ") or line == "E"
        ]
        if e_lines:
            self.reprcrash = _CrashLocation("\n".join(e_lines))

    def __str__(self):
        return self._text


class _Report:
    """The report shape LogXML consumes, built from the Rust TestReport
    (pytest's BaseReport surface: outcome flags + captured-section
    properties)."""

    def __init__(
        self,
        nodeid,
        when,
        outcome,
        duration,
        longrepr,
        location,
        sections,
        user_properties,
        wasxfail=None,
    ):
        self.nodeid = nodeid
        self.when = when
        self.passed = outcome == "passed"
        self.failed = outcome == "failed"
        self.skipped = outcome == "skipped"
        self.duration = duration
        self.longrepr = longrepr
        self.location = location
        self.sections = sections
        self.user_properties = user_properties
        if wasxfail is not None:
            self.wasxfail = wasxfail

    def _join_sections(self, prefix):
        return "\n".join(
            content for (header, content) in self.sections if header.startswith(prefix)
        )

    @property
    def capstdout(self):
        return self._join_sections("Captured stdout")

    @property
    def capstderr(self):
        return self._join_sections("Captured stderr")

    @property
    def caplog(self):
        return self._join_sections("Captured log")


class JunitState:
    """Session glue: the LogXML instance plus the per-item user properties
    recorded by the fixtures before reports reach LogXML."""

    def __init__(self):
        self.log_xml = None
        self.item_properties = {}  # nodeid -> [(name, value)]

    def configure(self, xmlpath, prefix, settings):
        def get(key, default):
            value = settings.get(key)
            return value if value not in (None, "") else default

        self.log_xml = LogXML(
            xmlpath,
            prefix,
            get("junit_suite_name", "pytest"),
            get("junit_logging", "no"),
            get("junit_duration_report", "total"),
            get("junit_family", "xunit2"),
            str(get("junit_log_passing_tests", "true")).strip().lower() not in ("false", "0", "no"),
        )
        self.log_xml.pytest_sessionstart()

    def record_item_property(self, nodeid, name, value):
        self.item_properties.setdefault(nodeid, []).append((name, value))

    def log_report(self, report_data):
        """Feed one Rust TestReport (as a dict) through LogXML."""
        nodeid = report_data["nodeid"]
        when = report_data["when"]
        outcome = report_data["outcome"]
        longrepr = report_data.get("longrepr")
        skip_location = report_data.get("skip_location")
        wasxfail = None

        if outcome == "xfailed":
            # pytest: an expected failure is a skipped report with .wasxfail.
            outcome = "skipped"
            wasxfail = longrepr or ""
        elif outcome == "xpassed":
            outcome = "passed"

        if outcome == "skipped" and wasxfail is None:
            # pytest skip longrepr is the (file, line, "Skipped: ...") tuple.
            filename, lineno = "", 0
            if skip_location:
                filename, _, lineno_str = skip_location.rpartition(":")
                try:
                    lineno = int(lineno_str)
                except ValueError:
                    filename = skip_location
            longrepr = (filename, lineno, f"Skipped: {longrepr or ''}")
        elif isinstance(longrepr, str):
            longrepr = _LongReprText(longrepr)

        report = _Report(
            nodeid=nodeid,
            when=when,
            outcome=outcome,
            duration=report_data.get("duration", 0.0),
            longrepr=longrepr,
            location=(
                report_data.get("file") or nodeid.partition("::")[0],
                report_data.get("line"),
                "",
            ),
            sections=report_data.get("sections") or [],
            user_properties=self.item_properties.get(nodeid, []),
            wasxfail=wasxfail,
        )
        if report_data.get("collect"):
            self.log_xml.pytest_collectreport(report)
        else:
            self.log_xml.pytest_runtest_logreport(report)

    def finish(self):
        """Write the XML file; returns its path for the terminal line."""
        self.log_xml.pytest_sessionfinish()
        return self.log_xml.logfile


state = JunitState()


def configure(xmlpath, prefix, settings):
    state.configure(xmlpath, prefix, settings)


def log_report(report_data):
    state.log_report(report_data)


def finish():
    return state.finish()


def _warn_incompatibility_with_xunit2(request, fixture_name):
    """PytestWarning (at the item's location) when the fixture is used
    under junit_family=xunit2."""
    if state.log_xml is not None and state.log_xml.family not in ("xunit1", "legacy"):
        request.node.warn(
            PytestWarning(
                f"{fixture_name} is incompatible with junit_family "
                f"'{state.log_xml.family}' (use 'legacy' or 'xunit1')"
            )
        )


@fixture
def record_property(request):
    """Add extra properties to the calling test.

    The fixture is callable with ``name, value``; the value is
    automatically XML-encoded.
    """
    _warn_incompatibility_with_xunit2(request, "record_property")
    nodeid = request.node.nodeid

    def append_property(name, value):
        state.record_item_property(nodeid, name, value)

    yield append_property


@fixture
def record_xml_attribute(request):
    """Add extra xml attributes to the tag for the calling test.

    The fixture is callable with ``name, value``; the value is
    automatically XML-encoded.
    """
    request.node.warn(
        PytestExperimentalApiWarning("record_xml_attribute is an experimental feature")
    )
    _warn_incompatibility_with_xunit2(request, "record_xml_attribute")

    def add_attr_noop(name, value):
        pass

    attr_func = add_attr_noop
    if state.log_xml is not None:
        node_reporter = state.log_xml.node_reporter(request.node.nodeid)
        attr_func = node_reporter.add_attribute

    yield attr_func


@fixture(scope="session")
def record_testsuite_property():
    """Record a new ``<property>`` tag as child of the root ``<testsuite>``.

    Suitable for writing global information regarding the entire test
    suite, and compatible with the xunit2 JUnit family."""
    __tracebackhide__ = True

    def record_func(name, value):
        """No-op function in case --junit-xml was not passed in the command-line."""
        __tracebackhide__ = True
        _check_record_param_type("name", name)

    if state.log_xml is not None:
        record_func = state.log_xml.add_global_property
    yield record_func
