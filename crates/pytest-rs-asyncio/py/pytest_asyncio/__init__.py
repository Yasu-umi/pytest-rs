"""The `pytest_asyncio` package shim: upstream suites import this."""

from .plugin import fixture, is_async_test

__version__ = "1.4.0"  # pytest-asyncio API version this shim tracks

__all__ = ("fixture", "is_async_test")
