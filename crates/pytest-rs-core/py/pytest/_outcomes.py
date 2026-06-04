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


def skip(reason=""):
    __tracebackhide__ = True
    raise Skipped(msg=reason)


def fail(reason="", pytrace=True):
    __tracebackhide__ = True
    raise Failed(msg=reason)


def xfail(reason=""):
    __tracebackhide__ = True
    raise XFailed(msg=reason)


def importorskip(modname, minversion=None, reason=None):
    import importlib

    try:
        mod = importlib.import_module(modname)
    except ImportError:
        raise Skipped(msg=reason or f"could not import {modname!r}") from None
    if minversion is not None:
        version = getattr(mod, "__version__", None)
        if version is None or version < minversion:
            raise Skipped(
                msg=f"module {modname!r} has __version__ {version}, required is: {minversion!r}"
            )
    return mod
