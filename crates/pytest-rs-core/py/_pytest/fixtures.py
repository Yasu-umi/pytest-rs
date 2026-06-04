import pytest

# Set on the pytest module by the Rust engine at startup (a pyo3 class).
FixtureRequest = getattr(pytest, "FixtureRequest", object)


class FixtureLookupError(LookupError):
    pass


from _pytest._stub import __getattr__  # noqa: E402, F401
