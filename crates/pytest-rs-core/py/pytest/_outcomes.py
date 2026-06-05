"""Outcome exceptions and helpers (skip/fail/xfail/importorskip)."""


class OutcomeException(BaseException):
    def __init__(self, msg=None):
        super().__init__(msg)
        self.msg = msg


class Skipped(OutcomeException):
    pass


class Failed(OutcomeException):
    pass


class XFailed(Failed):
    pass


def skip(reason="", allow_module_level=False):
    __tracebackhide__ = True
    exc = Skipped(msg=reason)
    exc.allow_module_level = allow_module_level
    raise exc


def fail(reason="", pytrace=True):
    __tracebackhide__ = True
    exc = Failed(msg=reason)
    exc.pytrace = pytrace
    raise exc


def xfail(reason=""):
    __tracebackhide__ = True
    raise XFailed(msg=reason)


class Exit(Exception):
    """Raised by pytest.exit (session abort)."""

    def __init__(self, msg="unknown reason", returncode=None):
        super().__init__(msg)
        self.msg = msg
        self.returncode = returncode


def exit(reason="", returncode=None):
    __tracebackhide__ = True
    raise Exit(reason, returncode)


def importorskip(modname, minversion=None, reason=None):
    import importlib

    try:
        mod = importlib.import_module(modname)
    except ImportError as exc:
        raise Skipped(msg=reason or f"could not import {modname!r}: {exc}") from None
    if minversion is not None:
        version = getattr(mod, "__version__", None)
        if version is None or version < minversion:
            raise Skipped(
                msg=f"module {modname!r} has __version__ {version}, required is: {minversion!r}"
            )
    return mod


# pytest parity: the raising helpers expose their exception type
# (`with pytest.raises(pytest.fail.Exception): ...`).
skip.Exception = Skipped
fail.Exception = Failed
xfail.Exception = XFailed
exit.Exception = Exit
