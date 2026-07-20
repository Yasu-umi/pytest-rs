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

    @classmethod
    def from_exc_info(cls, exc_info, _ispytest=False):
        """An ExceptionInfo from a (type, value, traceback) triple (upstream
        API; used e.g. by unittest.TestCase result callbacks)."""
        self = cls()
        self._set(*exc_info)
        return self

    def fill_unfilled(self, exc_info):
        """Fill from a (type, value, traceback) triple."""
        self._set(*exc_info)

    @property
    def _excinfo(self):
        """The raw (type, value, traceback) triple (upstream's storage
        attribute; some test code reads it directly instead of .type/.value/.tb)."""
        if self.type is None:
            return None
        return (self.type, self.value, self.tb)

    @property
    def typename(self):
        return self.type.__name__ if self.type else None

    @property
    def traceback(self):
        """The captured exception's traceback as a _pytest._code.Traceback
        (a list of TracebackEntry), for navigation/filtering by upstream tests."""
        from _pytest._code.code import Traceback

        return Traceback(self.tb)

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
        __tracebackhide__ = True
        value = str(self.value)
        try:
            notes = getattr(self.value, "__notes__", [])
        except Exception:
            notes = []
        if notes:
            value = "\n".join([value, *map(str, notes)])
        msg = (
            f"Regex pattern did not match.\n"
            f"  Expected regex: {regexp!r}\n"
            f"  Actual message: {value!r}"
        )
        if regexp == value:
            msg += "\n Did you mean to `re.escape()` the regex?"
        assert _re.search(regexp, value), msg
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


def _validate_exception(expected_exception):
    """Validate that expected_exception is a proper exception type or tuple of them."""
    if isinstance(expected_exception, tuple):
        for exc in expected_exception:
            _validate_exception(exc)
        return
    if isinstance(expected_exception, type) and issubclass(expected_exception, BaseException):
        return
    if isinstance(expected_exception, type):
        raise ValueError(f"Expected a BaseException type, but got '{expected_exception.__name__}'")
    raise TypeError(f"Expected a BaseException type, but got '{type(expected_exception).__name__}'")


class RaisesContext:
    def __init__(self, expected_exception, match=None, check=None):
        self.expected_exception = expected_exception
        if match is not None and match == "":
            import warnings

            from _pytest.warning_types import PytestWarning

            warnings.warn(
                PytestWarning(
                    "matching against an empty string will *always* pass. "
                    "If you want to check for an empty message you need to "
                    "pass '^$'. If you don't want to match you should pass "
                    "`None` or leave out the parameter."
                ),
                stacklevel=3,
            )
        self.match_expr = match
        self.check = check
        self.excinfo = None

    def __enter__(self):
        self.excinfo = ExceptionInfo()
        return self.excinfo

    def __exit__(self, exc_type, exc_value, tb) -> bool:
        __tracebackhide__ = True
        if exc_type is None:
            desc = (
                self.expected_exception if self.expected_exception is not None else "an exception"
            )
            fail(f"DID NOT RAISE {desc!r}")
        if self.expected_exception is not None and not issubclass(
            exc_type, self.expected_exception
        ):
            return False
        self.excinfo._set(exc_type, exc_value, tb)
        if self.match_expr is not None:
            try:
                _re.compile(self.match_expr)
            except _re.error as e:
                fail(
                    f"Invalid regex pattern provided to 'match': {e}",
                    pytrace=False,
                )
            self.excinfo.match(self.match_expr)
        if self.check is not None:
            if not self.check(exc_value):
                fail(f"{self.check!r} did not return True for the raised exception")
        return True


def raises(expected_exception=None, *args, **kwargs):
    __tracebackhide__ = True

    if not args:
        match = kwargs.pop("match", None)
        check = kwargs.pop("check", None)
        if set(kwargs) - {"expected_exception"}:
            msg = "Unexpected keyword arguments passed to pytest.raises: "
            msg += ", ".join(sorted(kwargs))
            msg += "\nUse context-manager form instead?"
            raise TypeError(msg)

        no_filter = (
            (
                expected_exception is None
                or (isinstance(expected_exception, tuple) and len(expected_exception) == 0)
            )
            and match is None
            and check is None
        )
        if no_filter:
            raise ValueError("You must specify at least one parameter to match on.")

        if expected_exception is None or (
            isinstance(expected_exception, tuple) and len(expected_exception) == 0
        ):
            return RaisesContext(expected_exception, match=match, check=check)

        if not expected_exception:
            raise ValueError(
                f"Expected an exception type or a tuple of exception types, but got `{expected_exception!r}`. "
                f"Raising exceptions is already understood as failing the test, so you don't need "
                f"any special code to say 'this should never raise an exception'."
            )

        _validate_exception(expected_exception)
        return RaisesContext(expected_exception, match=match, check=check)

    if not expected_exception:
        raise ValueError(
            f"Expected an exception type or a tuple of exception types, but got `{expected_exception!r}`. "
            f"Raising exceptions is already understood as failing the test, so you don't need "
            f"any special code to say 'this should never raise an exception'."
        )

    _validate_exception(expected_exception)
    func = args[0]
    if not callable(func):
        raise TypeError(f"{func!r} object (type: {type(func)}) must be callable")
    with RaisesContext(expected_exception) as excinfo:
        func(*args[1:], **kwargs)
    try:
        return excinfo
    finally:
        del excinfo


raises.Exception = fail.Exception  # type: ignore[attr-defined]
