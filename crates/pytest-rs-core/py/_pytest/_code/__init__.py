import inspect
import textwrap

from pytest._raises import ExceptionInfo as ExceptionInfo  # noqa: F401

from _pytest._code.code import Code as Code  # noqa: F401
from _pytest._code.code import Frame as Frame  # noqa: F401
from _pytest._code.code import Traceback as Traceback  # noqa: F401
from _pytest._code.code import TracebackEntry as TracebackEntry  # noqa: F401
from _pytest._code.code import filter_traceback as filter_traceback  # noqa: F401
from _pytest._code.code import get_real_func as get_real_func  # noqa: F401
from _pytest._code.code import getfslineno as getfslineno  # noqa: F401
from _pytest._code.code import getrawcode as getrawcode  # noqa: F401
from _pytest._stub import __getattr__  # noqa: F401


class Source:
    def __init__(self, obj=None):
        if obj is None:
            self.lines = []
        elif isinstance(obj, str):
            self.lines = textwrap.dedent(obj).splitlines()
        else:
            self.lines = textwrap.dedent(inspect.getsource(obj)).splitlines()

    def __str__(self):
        return "\n".join(self.lines)

    def strip(self):
        """Return a Source with leading/trailing blank lines removed (matches
        upstream's Source.strip(): only whitespace-only lines are trimmed —
        a real content line's own leading/trailing whitespace, e.g. a
        deliberately-indented first fnmatch_lines pattern, is preserved)."""
        start, end = 0, len(self.lines)
        while start < end and not self.lines[start].strip():
            start += 1
        while end > start and not self.lines[end - 1].strip():
            end -= 1
        stripped = Source()
        stripped.lines = self.lines[start:end]
        return stripped
