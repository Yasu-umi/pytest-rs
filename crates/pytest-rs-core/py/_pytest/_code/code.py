"""Minimal stubs for _pytest._code.code used by upstream test suites.

These are real classes (not _Unsupported stubs) so isinstance() and
attribute access work correctly for the subset of the API our suites need.
"""

from __future__ import annotations

from _pytest._stub import __getattr__  # noqa: F401


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


class TracebackEntry:
    """Minimal traceback entry."""

    def __init__(self, lines=()):
        self.lines = list(lines)
