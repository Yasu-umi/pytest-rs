"""pytest.raises (context manager and callable forms)."""

import re as _re
import sys
import traceback

from pytest._outcomes import fail


class ExceptionInfo[E: BaseException]:
    def __init__(self):
        self.type = None
        self.value = None
        self.tb = None

    def _set(self, type_, value, tb):
        self.type = type_
        self.value = value
        self.tb = tb

    @classmethod
    def for_later(cls):
        """An unfilled ExceptionInfo (upstream API, used by RaisesGroup)."""
        return cls()

    @classmethod
    def from_current(cls):
        """An ExceptionInfo for the exception currently being handled."""
        self = cls()
        self._set(*sys.exc_info())
        return self

    @classmethod
    def from_exception(cls, exception):
        """An ExceptionInfo wrapping an already-captured exception."""
        self = cls()
        self._set(type(exception), exception, exception.__traceback__)
        return self

    def fill_unfilled(self, exc_info):
        """Fill from a (type, value, traceback) triple."""
        self._set(*exc_info)

    @property
    def typename(self):
        return self.type.__name__ if self.type else None

    def errisinstance(self, exc):
        """Whether the captured exception is an instance of exc (or any in a
        tuple) — upstream ExceptionInfo.errisinstance."""
        return self.value is not None and isinstance(self.value, exc)

    def exconly(self, tryshort=False):
        """The exception's `type: message` line(s), like pytest's exconly.
        With tryshort, the rewritten-assert "AssertionError: " noise is
        stripped (upstream strips a leading striptext)."""
        text = "".join(traceback.format_exception_only(self.type, self.value)).strip()
        if tryshort:
            prefix = "AssertionError: "
            if text.startswith(prefix):
                text = text[len(prefix) :]
        return text

    def __repr__(self):
        # Upstream shape: "<ExceptionInfo ValueError('boom') tblen=2>" —
        # suites assert messages against str(excinfo).
        if self.value is None:
            return "<ExceptionInfo for raises contextmanager>"
        try:
            from _pytest._io.saferepr import saferepr

            shown = saferepr(self.value)
        except Exception:
            shown = repr(self.value)
        tblen = 0
        tb = self.tb
        while tb is not None:
            tblen += 1
            tb = tb.tb_next
        return f"<{type(self).__name__} {shown} tblen={tblen}>"

    def getrepr(self, showlocals=False, style="long", **kwargs):
        """A printable traceback representation (str() yields the formatted
        exception). Enough for pytest_internalerror and report longrepr; the
        style/showlocals knobs are accepted but not honored."""
        if self.value is None:
            return _ExceptionRepr("")
        text = "".join(traceback.format_exception(self.type, self.value, self.tb)).rstrip("\n")
        # reprcrash points at the deepest non-__tracebackhide__ frame, like
        # pytest (so e.g. importorskip's skip() reports the caller, not the
        # internal helper). Falls back to the deepest frame if all are hidden.
        path, lineno = "", 0
        tb = self.tb
        while tb is not None:
            frame = tb.tb_frame
            hidden = frame.f_locals.get("__tracebackhide__") or frame.f_globals.get(
                "__tracebackhide__"
            )
            if not hidden or not path:
                path = frame.f_code.co_filename
                lineno = tb.tb_lineno
            tb = tb.tb_next
        message = "".join(traceback.format_exception_only(self.type, self.value)).strip()
        return _ExceptionRepr(text, path=path, lineno=lineno, message=message)

    def match(self, regexp):
        # Upstream stringify_exception: PEP-678 __notes__ join the message.
        value = str(self.value)
        try:
            notes = getattr(self.value, "__notes__", [])
        except Exception:
            notes = []
        if notes:
            value = "\n".join([value, *map(str, notes)])
        if not _re.search(regexp, value):
            fail(f"Regex pattern did not match.\n Regex: {regexp!r}\n Input: {value!r}")
        return True


class _ReprCrash:
    def __init__(self, message, path="", lineno=0):
        self.message = message
        self.path = path
        self.lineno = lineno


class _ExceptionRepr:
    """Minimal TerminalRepr stand-in returned by ExceptionInfo.getrepr():
    str() and toterminal() render the traceback text; reprcrash carries the
    crash message and the deepest frame's path/lineno."""

    def __init__(self, text, path="", lineno=0, message=None):
        self.text = text
        if message is None:
            message = text.rstrip("\n").rsplit("\n", 1)[-1] if text else ""
        self.reprcrash = _ReprCrash(message, path=path, lineno=lineno)

    def __str__(self):
        return self.text

    def toterminal(self, tw):
        for line in self.text.split("\n"):
            tw.line(line)


class RaisesContext:
    def __init__(self, expected_exception, match=None):
        self.expected_exception = expected_exception
        self.match_expr = match
        self.excinfo = None

    def __enter__(self):
        self.excinfo = ExceptionInfo()
        return self.excinfo

    def __exit__(self, exc_type, exc_value, tb):
        __tracebackhide__ = True
        if exc_type is None:
            expected = getattr(self.expected_exception, "__name__", str(self.expected_exception))
            fail(f"DID NOT RAISE {expected}")
        if not issubclass(exc_type, self.expected_exception):
            return False
        self.excinfo._set(exc_type, exc_value, tb)
        if self.match_expr is not None:
            self.excinfo.match(self.match_expr)
        return True


def raises(expected_exception, *args, match=None, **kwargs):
    if args:
        func, *fargs = args
        with RaisesContext(expected_exception) as excinfo:
            func(*fargs, **kwargs)
        return excinfo
    return RaisesContext(expected_exception, match=match)
