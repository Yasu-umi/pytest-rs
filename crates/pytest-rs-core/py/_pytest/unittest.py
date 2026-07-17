from pytest._node import Function
from pytest._unittest import TestCaseFunction as _TestCaseFunctionMixin

from _pytest._stub import __getattr__  # noqa: E402, F401


class TestCaseFunction(_TestCaseFunctionMixin, Function):
    """A real Function item combining pytest._unittest's result-callback
    protocol mixin — see that module for why this isn't what the engine
    actually uses to run unittest tests."""
