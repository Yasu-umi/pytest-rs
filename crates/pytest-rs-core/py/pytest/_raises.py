"""pytest.raises (context manager and callable forms)."""

import re as _re

from pytest._outcomes import fail


class ExceptionInfo:
    def __init__(self):
        self.type = None
        self.value = None
        self.tb = None

    def _set(self, type_, value, tb):
        self.type = type_
        self.value = value
        self.tb = tb

    @property
    def typename(self):
        return self.type.__name__ if self.type else None

    def match(self, regexp):
        if not _re.search(regexp, str(self.value)):
            fail(f"Regex pattern did not match.\n Regex: {regexp!r}\n Input: {str(self.value)!r}")
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
