"""Terminal-reporting helpers ported from pytest's _pytest/terminal.py
(upstream unit-tests these directly)."""

import datetime


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


class TerminalReporter:
    """Trimmed port of upstream's TerminalReporter: only the summary-stats
    logic that upstream unit-tests directly (terminal output itself is the
    engine's)."""

    def __init__(self, config, file=None):
        self.config = config
        self.stats = {}
        self._session = None
        self._numcollected = 0
        self._main_color = None
        self._known_types = None
        self._progress_nodeids_reported = set()

    @property
    def reported_progress(self):
        """The amount of items reported in the progress so far."""
        return len(self._progress_nodeids_reported)

    @property
    def _is_last_item(self):
        assert self._session is not None
        return self.reported_progress == self._session.testscollected

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


from _pytest._stub import __getattr__  # noqa: E402, F401
