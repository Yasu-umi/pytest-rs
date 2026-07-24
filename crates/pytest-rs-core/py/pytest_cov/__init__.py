"""pytest-cov API shim provided by pytest-rs-cov.

Coverage measurement itself is Rust-native (sys.monitoring); this module
only mirrors the importable surface so suites referencing pytest_cov load.
"""


class CovError(Exception):
    """Base class for pytest-cov errors."""


class CovFailUnderError(CovError):
    """Raised when total coverage is below --cov-fail-under."""


class DistCovError(CovError):
    """Raised for invalid distributed-coverage configuration."""


class CentralCovContextWarning(Warning):
    pass


class DistCovContextWarning(Warning):
    pass


class CovDisabledWarning(Warning):
    pass


class CovReportWarning(Warning):
    pass


class CoverageWarning(Warning):
    """A warning from the native coverage measurement (mirrors
    coverage.py's own `coverage.exceptions.CoverageWarning`)."""
