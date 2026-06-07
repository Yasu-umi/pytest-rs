class _ReprCrash:
    """Upstream's reprcrash surface (path, lineno, message) recovered from
    the shim's formatted-string longrepr."""

    def __init__(self, message, path="", lineno=0):
        self.message = message
        self.path = path
        self.lineno = lineno

    def __str__(self):
        if self.path:
            return f"{self.path}:{self.lineno}: {self.message}"
        return self.message


class _ReprFileLoc:
    def __init__(self, path, lineno, message):
        self.path = path
        self.lineno = lineno
        self.message = message

    def __str__(self):
        return f"{self.path}:{self.lineno}: {self.message}"


class _ReprEntry:
    def __init__(self, reprfileloc):
        self.reprfileloc = reprfileloc


class _ReprTraceback:
    def __init__(self, reprentries):
        self.reprentries = reprentries


class _LongRepr(str):
    """The longrepr string with upstream's `.reprcrash` / `.chain`
    attributes, so consumers like pytest-pretty's ' - <crash message>'
    suffixes and failure table work."""

    def _location(self):
        """(path, lineno, error) from the trailing 'file.py:NN: Error'
        location line."""
        for line in reversed(self.splitlines()):
            parts = line.split(":", 2)
            if len(parts) >= 2 and parts[0].endswith(".py") and parts[1].isdigit():
                error = parts[2].strip() if len(parts) > 2 else ""
                return parts[0], int(parts[1]), error
        return "", 0, ""

    @property
    def reprcrash(self):
        message = None
        for line in self.splitlines():
            if line.startswith(("E ", "E\t")):
                message = line[1:].strip()
                break
        if message is None:
            lines = [line for line in self.splitlines() if line.strip()]
            message = lines[-1].strip() if lines else ""
        path, lineno, _ = self._location()
        return _ReprCrash(message, path, lineno)

    @property
    def chain(self):
        path, lineno, error = self._location()
        entry = _ReprEntry(_ReprFileLoc(path, lineno, error))
        return [(_ReprTraceback([entry]), None, None)]


class BaseReport:
    # pytest's BaseReport surface used by report consumers (junitxml):
    # captured-output properties derived from the (header, content)
    # sections list.
    sections: list = []

    def _join_sections(self, prefix):
        return "\n".join(
            content for (header, content) in self.sections if header.startswith(prefix)
        )

    @property
    def capstdout(self):
        return self._join_sections("Captured stdout")

    @property
    def capstderr(self):
        return self._join_sections("Captured stderr")

    @property
    def caplog(self):
        return self._join_sections("Captured log")

    @property
    def passed(self):
        return getattr(self, "outcome", None) == "passed"

    @property
    def failed(self):
        return getattr(self, "outcome", None) == "failed"

    @property
    def skipped(self):
        return getattr(self, "outcome", None) == "skipped"

    @property
    def count_towards_summary(self):
        return True

    @property
    def fspath(self):
        return getattr(self, "nodeid", "").split("::")[0]

    @property
    def head_line(self):
        """The FAILURES-section header for this report (upstream derives it
        from the location domain)."""
        location = getattr(self, "location", None)
        if location and location[2]:
            return location[2]
        return getattr(self, "nodeid", "")

    def toterminal(self, tw):
        """Write longrepr to a TerminalWriter (upstream delegates to the
        repr object tree; the shim's longrepr is a formatted string).
        Upstream's first traceback entry opens with a blank line after the
        failure banner."""
        longrepr = getattr(self, "longrepr", None)
        if longrepr is None:
            return
        tw.line("")
        for line in str(longrepr).split("\n"):
            markup = (
                {"red": True, "bold": True} if line.startswith(("E ", "E\t")) or line == "E" else {}
            )
            tw.line(line, **markup)


def _set_report_attrs(report, kwargs):
    for name, value in kwargs.items():
        if name == "longrepr" and isinstance(value, str) and not isinstance(value, _LongRepr):
            value = _LongRepr(value)
        setattr(report, name, value)


class TestReport(BaseReport):
    def __init__(self, **kwargs):
        _set_report_attrs(self, kwargs)


class CollectReport(BaseReport):
    when = "collect"

    def __init__(self, **kwargs):
        _set_report_attrs(self, kwargs)


from _pytest._stub import __getattr__  # noqa: E402, F401
