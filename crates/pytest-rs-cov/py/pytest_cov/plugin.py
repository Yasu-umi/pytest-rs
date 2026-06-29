"""pytest_cov.plugin surface shim: coverage runs natively in Rust, so the
upstream plugin classes exist only as raising stubs."""

import os as _os
import sys as _sys

try:
    import pytest as _pytest
except ModuleNotFoundError:
    _pytest = None

try:
    import coverage as _coverage_module
except ModuleNotFoundError:
    _coverage_module = None

if _pytest is not None:

    @_pytest.fixture
    def cov():
        """Return the active Coverage instance, or None when not measuring."""
        if not _os.environ.get("PYTEST_RS_COV_ACTIVE") or _coverage_module is None:
            return None
        return _coverage_module.Coverage()

    @_pytest.fixture
    def no_cover():
        """Pause sys.monitoring coverage for the duration of this test."""
        tool_id_str = _os.environ.get("PYTEST_RS_COV_TOOL_ID")
        saved_child = _os.environ.pop("PYTEST_RS_COV_CHILD", None)
        if tool_id_str is not None:
            tool_id = int(tool_id_str)
            monitoring = _sys.monitoring
            saved_events = monitoring.get_events(tool_id)
            monitoring.set_events(tool_id, 0)
            yield
            monitoring.set_events(tool_id, saved_events)
        else:
            yield
        if saved_child is not None:
            _os.environ["PYTEST_RS_COV_CHILD"] = saved_child


class CovPlugin:
    def __init__(self, *args, start=True, **kwargs):
        if start:
            raise NotImplementedError(
                "pytest_cov.plugin.CovPlugin is not supported by pytest-rs "
                "(coverage is measured natively via sys.monitoring)"
            )

    def pytest_runtestloop(self, session):
        pass

    def pytest_terminal_summary(self, terminalreporter):
        pass


class StoreReport:
    def __init__(self, *args, **kwargs):
        raise NotImplementedError("pytest_cov.plugin.StoreReport is not supported by pytest-rs")
