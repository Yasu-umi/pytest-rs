from pytest import ExitCode, UsageError  # noqa: F401


class Config:
    """Stub config type (mostly used for annotations upstream)."""


class PytestPluginManager:
    """Stub plugin manager (pluggy is not used by pytest-rs)."""


def main(args=None, plugins=None):
    raise NotImplementedError("_pytest.config.main is not supported by pytest-rs")
