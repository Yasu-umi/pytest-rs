import pytest

# Set on the pytest module by the Rust engine at startup (a pyo3 class).
FixtureRequest = getattr(pytest, "FixtureRequest", object)


from pytest._fixtures import FixtureLookupError as FixtureLookupError  # noqa: E402


class FixtureFunctionDefinition:
    """pytest 8.4+ wraps @pytest.fixture functions in this. pytest-rs marks
    fixtures with recorded metadata instead, so nothing is ever an instance —
    but it must be a real class: hypothesis isinstance()s against it at
    @given application time (an _Unsupported stub raises TypeError there)."""


from _pytest._stub import __getattr__  # noqa: E402, F401
