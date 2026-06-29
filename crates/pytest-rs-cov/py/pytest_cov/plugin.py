"""pytest_cov.plugin surface shim: coverage runs natively in Rust, so the
upstream plugin classes exist only as raising stubs."""


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
