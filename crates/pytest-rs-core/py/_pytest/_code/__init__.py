import inspect
import textwrap

from pytest._raises import ExceptionInfo as ExceptionInfo  # noqa: F401

from _pytest._code.code import Code as Code  # noqa: F401
from _pytest._code.code import Frame as Frame  # noqa: F401
from _pytest._code.code import Traceback as Traceback  # noqa: F401
from _pytest._code.code import TracebackEntry as TracebackEntry  # noqa: F401
from _pytest._code.code import filter_traceback as filter_traceback  # noqa: F401
from _pytest._code.code import getrawcode as getrawcode  # noqa: F401
from _pytest._stub import __getattr__  # noqa: F401


class Source:
    def __init__(self, obj=None):
        if obj is None:
            self.lines = []
        elif isinstance(obj, str):
            self.lines = textwrap.dedent(obj).strip().splitlines()
        else:
            self.lines = textwrap.dedent(inspect.getsource(obj)).strip().splitlines()

    def __str__(self):
        return "\n".join(self.lines)

    def strip(self):
        """Return a Source with leading/trailing blank lines removed. Our
        constructor already strips string input, so the lines are clean;
        this exists so callers can write ``Source(x).strip().lines`` (the
        shape LineMatcher._getlines relies on)."""
        return self
