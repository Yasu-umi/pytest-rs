"""pytest.raises (context manager and callable forms)."""

import re as _re

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

    def fill_unfilled(self, exc_info):
        """Fill from a (type, value, traceback) triple."""
        self._set(*exc_info)

    @property
    def typename(self):
        return self.type.__name__ if self.type else None

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
