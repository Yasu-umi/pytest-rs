"""pytest_cov.plugin surface shim: coverage runs natively in Rust, so the
upstream plugin classes exist only as raising stubs."""

import os as _os
import sys as _sys

try:
    import pytest as _pytest
except ModuleNotFoundError:
    _pytest = None

if _pytest is not None:

    @_pytest.fixture
    def cov():
        """Return the active Coverage instance, or None when not measuring."""
        if not _os.environ.get("PYTEST_RS_COV_ACTIVE"):
            return None
        # Deferred: importing the real `coverage` package (a substantial
        # module tree) unconditionally at plugin-module import time cost a
        # measurable chunk of startup on every run, even the overwhelming
        # majority that never request this fixture at all (this module is
        # imported by pytest_configure regardless of --cov, so cov/no_cover
        # exist for test_funcarg_not_active-style requests without --cov).
        try:
            import coverage as _coverage_module
        except ModuleNotFoundError:
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
