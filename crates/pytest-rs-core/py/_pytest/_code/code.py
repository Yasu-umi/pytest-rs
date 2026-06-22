"""Minimal stubs for _pytest._code.code used by upstream test suites.

These are real classes (not _Unsupported stubs) so isinstance() and
attribute access work correctly for the subset of the API our suites need.
"""

from __future__ import annotations

from collections.abc import Callable
from pathlib import Path
from types import TracebackType
from typing import Any

try:
    import pluggy
except ModuleNotFoundError:
    # The shim is self-contained (pytest-rs ships its own pluginmanager), so
    # real pluggy may be absent — e.g. the standalone binary's embedded
    # interpreter in CI. filter_traceback then simply has no pluggy frames to
    # hide; it still filters them when pluggy IS importable (conformance runs).
    pluggy = None

import _pytest
from _pytest._stub import __getattr__  # noqa: F401
from _pytest.pathlib import absolutepath


class ReprFileLocation:
    """Minimal crash location (path + lineno + message)."""

    def __init__(self, path="", lineno=0, message=""):
        self.path = path
        self.lineno = lineno
        self.message = message

    def __str__(self):
        return f"{self.path}:{self.lineno}: {self.message}"

    def toterminal(self, tw):
        tw.line(str(self))


class ReprTraceback:
    """Minimal traceback repr."""

    def __init__(self, reprentries=(), extraline=None, style="long"):
        self.reprentries = list(reprentries)
        self.extraline = extraline
        self.style = style

    def toterminal(self, tw):
        for entry in self.reprentries:
            if hasattr(entry, "toterminal"):
                entry.toterminal(tw)
        if self.extraline:
            tw.line(self.extraline)


class ExceptionChainRepr:
    """Minimal exception chain representation.

    Compatible with real pytest's ExceptionChainRepr for isinstance() checks
    and the attribute accesses upstream tests make (.reprcrash, .reprtraceback,
    .longreprtext).
    """

    def __init__(self, chain=()):
        # chain: list of (ReprTraceback, ReprFileLocation | None, description | None)
        self.chain = list(chain)
        # Last traceback/crash in the chain (the innermost exception).
        self.reprtraceback = self.chain[-1][0] if self.chain else ReprTraceback()
        self.reprcrash = self.chain[-1][1] if self.chain else None

    @property
    def longreprtext(self):
        parts = []
        for tb, crash, desc in self.chain:
            if desc:
                parts.append(desc)
            if hasattr(tb, "reprentries"):
                for entry in tb.reprentries:
                    parts.append(str(getattr(entry, "lines", entry)))
            if crash:
                parts.append(str(crash))
        return "\n".join(parts)

    def __str__(self):
        return self.longreprtext

    def toterminal(self, tw):
        for tb, crash, desc in self.chain:
            if hasattr(tb, "toterminal"):
                tb.toterminal(tw)
            if crash and hasattr(crash, "toterminal"):
                crash.toterminal(tw)


class FormattedExcinfo:
    """Minimal stub for FormattedExcinfo."""

    pass


def getrawcode(obj, trycall: bool = True):
    """Return the code object for the given function (mirrors
    _pytest._code.source.getrawcode)."""
    try:
        return obj.__code__
    except AttributeError:
        pass
    if trycall:
        call = getattr(obj, "__call__", None)  # noqa: B004
        if call and not isinstance(obj, type):
            return getrawcode(call, trycall=False)
    raise TypeError(f"could not get code object for {obj!r}")


class Code:
    """Wrapper around a Python code object."""

    __slots__ = ("raw",)

    def __init__(self, obj):
        self.raw = obj

    @classmethod
    def from_function(cls, obj):
        return cls(getrawcode(obj))

    def __eq__(self, other):
        return self.raw == other.raw

    __hash__ = None  # type: ignore[assignment]

    @property
    def firstlineno(self) -> int:
        return self.raw.co_firstlineno - 1

    @property
    def name(self) -> str:
        return self.raw.co_name

    @property
    def path(self):
        """A Path to the source, or the raw co_filename str when the file
        does not exist (dynamically generated code, deleted file)."""
        if not self.raw.co_filename:
            return ""
        try:
            p = absolutepath(self.raw.co_filename)
            if not p.exists():
                raise OSError("path check failed.")
            return p
        except OSError:
            return self.raw.co_filename


def get_real_func(obj):
    import functools
    import inspect

    obj = inspect.unwrap(obj)
    if isinstance(obj, functools.partial):
        obj = obj.func
    return obj


def getfslineno(obj):
    import inspect

    obj = get_real_func(obj)
    if hasattr(obj, "place_as"):
        obj = obj.place_as
    try:
        code = Code.from_function(obj)
    except TypeError:
        try:
            fn = inspect.getsourcefile(obj) or inspect.getfile(obj)
        except TypeError:
            return "", -1
        fspath = (fn and absolutepath(fn)) or ""
        lineno = -1
        if fspath:
            try:
                _, lineno = inspect.findsource(obj)
            except OSError:
                pass
        return fspath, lineno
    return code.path, code.firstlineno


class Frame:
    """Wrapper around a Python frame."""

    __slots__ = ("raw",)

    def __init__(self, frame):
        self.raw = frame

    @property
    def lineno(self) -> int:
        return self.raw.f_lineno - 1

    @property
    def f_globals(self):
        return self.raw.f_globals

    @property
    def f_locals(self):
        return self.raw.f_locals

    @property
    def code(self) -> Code:
        return Code(self.raw.f_code)


class TracebackEntry:
    """A single entry in a Traceback."""

    __slots__ = ("_rawentry", "_repr_style")

    def __init__(self, rawentry, repr_style=None):
        self._rawentry = rawentry
        self._repr_style = repr_style

    def with_repr_style(self, repr_style) -> TracebackEntry:
        return TracebackEntry(self._rawentry, repr_style)

    @property
    def lineno(self) -> int:
        return self._rawentry.tb_lineno - 1

    @property
    def frame(self) -> Frame:
        return Frame(self._rawentry.tb_frame)

    @property
    def relline(self) -> int:
        return self.lineno - self.frame.code.firstlineno

    def __repr__(self) -> str:
        return f"<TracebackEntry {self.frame.code.path}:{self.lineno + 1}>"

    def __str__(self) -> str:
        name = self.frame.code.raw.co_name
        try:
            import linecache

            path = self.frame.code.raw.co_filename
            line = linecache.getline(path, self.lineno + 1).strip()
            if not line:
                line = "???"
        except Exception:
            line = "???"
        return f"  File '{self.path}':{self.lineno + 1} in {name}\n  {line}\n"

    @property
    def path(self):
        """Path to the source code (a Path, or a str for generated/missing files)."""
        return self.frame.code.path

    @property
    def locals(self):
        return self.frame.f_locals

    def getfirstlinesource(self) -> int:
        return self.frame.code.firstlineno

    def ishidden(self, excinfo) -> bool:
        """True when the frame sets __tracebackhide__ (truthy, or a callable
        returning truthy when passed the ExceptionInfo)."""
        tbh: bool | Callable[[Any], bool] = False
        for maybe_ns_dct in (self.frame.f_locals, self.frame.f_globals):
            try:
                tbh = maybe_ns_dct["__tracebackhide__"]
            except Exception:
                pass
            else:
                break
        if tbh and callable(tbh):
            return tbh(excinfo)
        return tbh

    @property
    def name(self) -> str:
        """co_name of the underlying code."""
        return self.frame.code.raw.co_name


class Traceback(list):
    """Higher-level access to a chain of TracebackEntry objects."""

    def __init__(self, tb):
        if isinstance(tb, TracebackType):

            def f(cur):
                while cur is not None:
                    yield TracebackEntry(cur)
                    cur = cur.tb_next

            super().__init__(f(tb))
        else:
            super().__init__(tb)


if pluggy is not None:
    _pluggy_dir = Path(pluggy.__file__.rstrip("oc"))
    # pluggy is either a package or a single module.
    if _pluggy_dir.name == "__init__.py":
        _pluggy_dir = _pluggy_dir.parent
    _PLUGGY_DIR: Path | None = _pluggy_dir
else:
    _PLUGGY_DIR = None
_PYTEST_DIR = Path(_pytest.__file__).parent


def filter_traceback(entry: TracebackEntry) -> bool:
    """Return True if a TracebackEntry should be shown. Hides dynamically
    generated code and frames internal to pytest/pluggy."""
    raw_filename = entry.frame.code.raw.co_filename
    is_generated = "<" in raw_filename and ">" in raw_filename
    if is_generated:
        return False
    # entry.path may be a str for a non-existing file (#1133); Path() still works.
    p = Path(entry.path)
    parents = p.parents
    if _PLUGGY_DIR is not None and _PLUGGY_DIR in parents:
        return False
    if _PYTEST_DIR in parents:
        return False
    return True
