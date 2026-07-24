"""Outcome exceptions and helpers (skip/fail/xfail/importorskip)."""

import importlib
import warnings
from typing import NoReturn


class OutcomeException(BaseException):
    def __init__(self, msg=None):
        if msg is not None and not isinstance(msg, str):
            raise TypeError(
                f"{type(self).__name__} expected string as 'msg' parameter, got "
                f"'{type(msg).__name__}' instead.\nPerhaps you meant to use a mark?"
            )
        super().__init__(msg)
        self.msg = msg


class Skipped(OutcomeException):
    __module__ = "builtins"

    # Set by pytest.skip() on the instance it raises (declared for the typed
    # callers below, not created at class level).
    allow_module_level: bool


class Failed(OutcomeException):
    __module__ = "builtins"

    # Set by pytest.fail() on the instance it raises.
    pytrace: bool


class XFailed(Failed):
    pass


class _Skip:
    """pytest.skip implemented as a callable class so the frame that raises
    Skipped is `_Skip.__call__` (upstream introspects co_qualname and expects
    the frame to be __tracebackhide__-hidden — test_skip_simple)."""

    Exception = Skipped

    # NoReturn on this family (upstream types them the same way) is what lets a
    # caller end a branch with `pytest.fail(...)` instead of a return: without
    # it mypy reports a spurious "Missing return statement" for the enclosing
    # function.
    def __call__(self, reason: str = "", allow_module_level: bool = False) -> NoReturn:
        __tracebackhide__ = True
        exc = Skipped(msg=reason)
        exc.allow_module_level = allow_module_level
        raise exc


skip = _Skip()


def fail(reason: str = "", pytrace: bool = True) -> NoReturn:
    __tracebackhide__ = True
    exc = Failed(msg=reason)
    exc.pytrace = pytrace
    raise exc


def xfail(reason: str = "") -> NoReturn:
    __tracebackhide__ = True
    raise XFailed(msg=reason)


class Exit(Exception):
    """Raised by pytest.exit (session abort)."""

    def __init__(self, msg="unknown reason", returncode=None):
        super().__init__(msg)
        self.msg = msg
        self.returncode = returncode


def exit(reason: str = "", returncode: int | None = None) -> NoReturn:
    __tracebackhide__ = True
    raise Exit(reason, returncode)


def importorskip(modname, minversion=None, reason=None, *, exc_type=None):
    __tracebackhide__ = True
    # Validate module name: real pytest raises SyntaxError for invalid names
    # (e.g. spaces or = signs that can never be module names).
    if not all(part.isidentifier() for part in modname.split(".")):
        raise SyntaxError(f"Not a valid module name: {modname!r}")

    try:
        mod = importlib.import_module(modname)
    except ImportError as exc:
        if exc_type is not None and not isinstance(exc, exc_type):
            raise
        # Distinguish ModuleNotFoundError (module not installed, no warning)
        # from ImportError (module found but failed during import → deprecation
        # warning; real pytest will drop this behaviour in a future version).
        if exc_type is None and not isinstance(exc, ModuleNotFoundError):
            from pytest._warning_types import PytestDeprecationWarning

            warnings.warn(
                f"Module {modname!r} was found, but when imported by pytest it raised:\n"
                f"      {exc!r}\n"
                "In the future only ModuleNotFoundError will be caught. "
                "Pass `exc_type=ImportError` to suppress this warning.",
                PytestDeprecationWarning,
                stacklevel=2,
            )
        skipped = Skipped(msg=reason or f"could not import {modname!r}: {exc}")
        skipped.allow_module_level = True
        raise skipped from None
    if minversion is not None:
        version = getattr(mod, "__version__", None)
        if version is None or version < minversion:
            skipped = Skipped(
                msg=f"module {modname!r} has __version__ {version}, required is: {minversion!r}"
            )
            skipped.allow_module_level = True
            raise skipped
    return mod


# pytest parity: the raising helpers expose their exception type
# (`with pytest.raises(pytest.fail.Exception): ...`). skip exposes it as a
# class attribute on _Skip; fail/xfail are functions so set it here.
fail.Exception = Failed  # type: ignore[attr-defined]
xfail.Exception = XFailed  # type: ignore[attr-defined]
exit.Exception = Exit  # type: ignore[attr-defined]
