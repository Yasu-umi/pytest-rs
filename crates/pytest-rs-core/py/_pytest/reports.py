from _pytest._code.code import ExceptionChainRepr as _ExceptionChainRepr  # noqa: F401


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
        self.lines: list[str] = []


class _ReprTraceback:
    def __init__(self, reprentries):
        self.reprentries = reprentries


class _LongRepr(str, _ExceptionChainRepr):
    """The longrepr string with upstream's `.reprcrash` / `.chain`
    attributes, so consumers like pytest-pretty's ' - <crash message>'
    suffixes and failure table work. Inherits from ExceptionChainRepr so
    isinstance(longrepr, ExceptionChainRepr) checks pass."""

    def __new__(cls, value=""):
        return str.__new__(cls, value)

    def __init__(self, value=""):
        pass

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
    def reprtraceback(self):
        lines = self.splitlines()
        entry = _ReprEntry(_ReprFileLoc("", 0, ""))
        entry.lines = list(lines)
        return _ReprTraceback([entry])

    @property
    def reprcrash(self):
        message_lines = []
        collecting = False
        for line in self.splitlines():
            if line.startswith(("E ", "E\t")):
                collecting = True
                message_lines.append(line[1:].strip())
            elif collecting:
                break
        if message_lines:
            message = "\n".join(message_lines)
        else:
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

    def __init__(self, **kwargs):
        _set_report_attrs(self, kwargs)

    def _join_sections(self, prefix):
        return "".join(content for (header, content) in self.sections if header.startswith(prefix))

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

    @property
    def longreprtext(self):
        """Return the full string representation of longrepr."""
        longrepr = getattr(self, "longrepr", None)
        if longrepr is None:
            return ""
        if isinstance(longrepr, tuple):
            return str(longrepr[2]) if len(longrepr) >= 3 else str(longrepr)
        try:
            return str(longrepr)
        except Exception:
            return "<unprintable longrepr>"

    def toterminal(self, tw):
        """Write longrepr to a TerminalWriter (upstream delegates to the
        repr object tree; the shim's longrepr is a formatted string).
        Upstream's first traceback entry opens with a blank line after the
        failure banner; the shim's longrepr already carries that leading
        blank, so only synthesize one when it is absent (otherwise a
        delegated reporter — pytest-pretty, sugar — renders a double blank)."""
        longrepr = getattr(self, "longrepr", None)
        if longrepr is None:
            return
        text = str(longrepr)
        if not text.startswith("\n"):
            tw.line("")
        for line in text.split("\n"):
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
    # Prevent collection of an imported `TestReport` as a test class (it has a
    # custom __init__); mirrors _pytest.reports.TestReport.__test__.
    __test__ = False

    def __init__(self, **kwargs):
        _set_report_attrs(self, kwargs)
        if not hasattr(self, "sections"):
            self.sections = []
        if not hasattr(self, "keywords"):
            self.keywords = {}

    def __repr__(self) -> str:
        return (
            f"<{self.__class__.__name__} {getattr(self, 'nodeid', '')!r}"
            f" when={getattr(self, 'when', '')!r}"
            f" outcome={getattr(self, 'outcome', '')!r}>"
        )

    def _to_json(self):
        longrepr = None
        if getattr(self, "longrepr", None) is not None:
            longrepr = str(self.longrepr)
        d = {
            "nodeid": getattr(self, "nodeid", ""),
            "when": getattr(self, "when", ""),
            "outcome": getattr(self, "outcome", ""),
            "longrepr": longrepr,
            "sections": list(getattr(self, "sections", [])),
            "keywords": dict(getattr(self, "keywords", {})),
        }
        for k, v in vars(self).items():
            if k not in d:
                try:
                    d[k] = v
                except Exception:
                    pass
        return d

    @classmethod
    def _from_json(cls, data):
        return cls(
            nodeid=data.get("nodeid", ""),
            when=data.get("when", ""),
            outcome=data.get("outcome", ""),
            longrepr=data.get("longrepr"),
            sections=data.get("sections") or [],
            keywords=data.get("keywords") or {},
            **{
                k: v
                for k, v in data.items()
                if k not in ("nodeid", "when", "outcome", "longrepr", "sections", "keywords")
            },
        )


class CollectReport(BaseReport):
    when = "collect"

    def __init__(
        self, nodeid=None, outcome=None, longrepr=None, result=None, sections=(), **kwargs
    ):
        if nodeid is not None:
            kwargs["nodeid"] = nodeid
        if outcome is not None:
            kwargs["outcome"] = outcome
        kwargs["longrepr"] = longrepr
        if result is not None:
            kwargs["result"] = result
        if sections:
            kwargs["sections"] = list(sections)
        _set_report_attrs(self, kwargs)
        if not hasattr(self, "sections"):
            self.sections = []

    def _to_json(self):
        longrepr = None
        if getattr(self, "longrepr", None) is not None:
            longrepr = str(self.longrepr)
        d = {
            "nodeid": getattr(self, "nodeid", ""),
            "outcome": getattr(self, "outcome", ""),
            "longrepr": longrepr,
            "result": [],
            "sections": list(getattr(self, "sections", [])),
        }
        for k, v in vars(self).items():
            if k not in d:
                try:
                    d[k] = v
                except Exception:
                    pass
        return d

    @classmethod
    def _from_json(cls, data):
        return cls(
            nodeid=data.get("nodeid", ""),
            outcome=data.get("outcome", ""),
            longrepr=data.get("longrepr"),
            result=data.get("result") or [],
            sections=data.get("sections") or [],
            **{
                k: v
                for k, v in data.items()
                if k not in ("nodeid", "outcome", "longrepr", "result", "sections")
            },
        )


from _pytest._stub import __getattr__  # noqa: E402, F401
