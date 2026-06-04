from pytest import ExitCode, UsageError  # noqa: F401


class Config:
    """Stub config type (mostly used for annotations upstream)."""

    VERBOSITY_ASSERTIONS = "assertions"
    VERBOSITY_TEST_CASES = "test_cases"
    VERBOSITY_SUBTESTS = "subtests"


class PytestPluginManager:
    """Stub plugin manager (pluggy is not used by pytest-rs)."""


def main(args=None, plugins=None):
    raise NotImplementedError("_pytest.config.main is not supported by pytest-rs")


from _pytest._stub import __getattr__  # noqa: E402, F401
