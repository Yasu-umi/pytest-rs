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


from _pytest._stub import __getattr__  # noqa: E402, F401
