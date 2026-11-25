"""A hook exception that has to end the session with INTERNALERROR.

Upstream calls the reporting hooks of the runtest protocol
(`pytest_runtest_makereport`, `pytest_runtest_logreport`,
`pytest_report_teststatus`, `pytest_runtest_logfinish`,
`pytest_exception_interact`, `pytest_warning_recorded`) with no
try/except, so an exception from any hookimpl propagates to
wrap_session's INTERNALERROR handler (exit code 3). pytest-rs calls them
from places that cannot unwind the run loop (Rust and Python alike), so
they hand the exception over here, and the run loop ends the session with
the first one once the current item is done.
"""

from __future__ import annotations


class DeferredInternalError:
    _pending: BaseException | None = None

    @classmethod
    def defer(cls, exc: BaseException) -> bool:
        """Keep `exc` (the first one wins); False for `pytest.exit()`,
        which is not an internal error."""
        from pytest._outcomes import Exit

        if isinstance(exc, Exit) or not isinstance(exc, Exception):
            return False
        if cls._pending is None:
            cls._pending = exc
        return True

    @classmethod
    def pending(cls) -> bool:
        return cls._pending is not None

    @classmethod
    def take(cls) -> BaseException | None:
        exc, cls._pending = cls._pending, None
        return exc


class InternalErrorRepr:
    """The `excrepr` handed to pytest_internalerror: prints as pytest-rs's
    own INTERNALERROR traceback (the frames it shows match upstream's), and
    otherwise behaves like the ExceptionInfo repr it wraps."""

    def __init__(self, text: str, excrepr: object) -> None:
        self._text = text
        self._excrepr = excrepr

    def __str__(self) -> str:
        return self._text

    def __getattr__(self, name: str) -> object:
        return getattr(self._excrepr, name)
