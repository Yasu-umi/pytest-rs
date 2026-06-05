"""pytest_cov.plugin surface shim: coverage runs natively in Rust, so the
upstream plugin classes exist only as raising stubs."""


class CovPlugin:
    def __init__(self, *args, **kwargs):
        raise NotImplementedError(
            "pytest_cov.plugin.CovPlugin is not supported by pytest-rs "
            "(coverage is measured natively via sys.monitoring)"
        )


class StoreReport:
    def __init__(self, *args, **kwargs):
        raise NotImplementedError("pytest_cov.plugin.StoreReport is not supported by pytest-rs")
