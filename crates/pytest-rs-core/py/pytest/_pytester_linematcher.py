"""pytester LineMatcher (split out of _pytester.py)."""

from pytest._outcomes import fail


class LineMatcher:
    """Flexible matching of text (port of upstream pytester.LineMatcher).

    Built from a list of lines without trailing newlines; the various
    matchers log their match/no-match trail so failures show exactly which
    pattern stopped matching and against which lines.
    """

    def __init__(self, lines):
        self.lines = lines
        self._log_output = []

    def __str__(self):
        return "\n".join(self.lines)

    def str(self):
        return str(self)

    def _getlines(self, lines2):
        from _pytest._code import Source

        if isinstance(lines2, str):
            lines2 = Source(lines2)
        if isinstance(lines2, Source):
            lines2 = lines2.strip().lines
        return lines2

    def fnmatch_lines_random(self, lines2):
        __tracebackhide__ = True
        import fnmatch

        self._match_lines_random(lines2, fnmatch.fnmatch)

    def re_match_lines_random(self, lines2):
        __tracebackhide__ = True
        import re

        self._match_lines_random(lines2, lambda name, pat: bool(re.match(pat, name)))

    def _match_lines_random(self, lines2, match_func):
        __tracebackhide__ = True
        lines2 = self._getlines(lines2)
        for line in lines2:
            for x in self.lines:
                if line == x or match_func(x, line):
                    self._log("matched: ", repr(line))
                    break
            else:
                msg = f"line {line!r} not found in output"
                self._log(msg)
                self._fail(msg)

    def get_lines_after(self, fnline):
        import fnmatch

        for i, line in enumerate(self.lines):
            if fnline == line or fnmatch.fnmatch(line, fnline):
                return self.lines[i + 1 :]
        raise ValueError(f"line {fnline!r} not found in output")

    def _log(self, *args):
        self._log_output.append(" ".join(str(x) for x in args))

    @property
    def _log_text(self):
        return "\n".join(self._log_output)

    def fnmatch_lines(self, lines2, *, consecutive=False):
        __tracebackhide__ = True
        import fnmatch

        self._match_lines(lines2, fnmatch.fnmatch, "fnmatch", consecutive=consecutive)

    def re_match_lines(self, lines2, *, consecutive=False):
        __tracebackhide__ = True
        import re

        self._match_lines(
            lines2,
            lambda name, pat: bool(re.match(pat, name)),
            "re.match",
            consecutive=consecutive,
        )

    def _match_lines(self, lines2, match_func, match_nickname, *, consecutive=False):
        import collections.abc

        if not isinstance(lines2, collections.abc.Sequence):
            raise TypeError(f"invalid type for lines2: {type(lines2).__name__}")
        lines2 = self._getlines(lines2)
        lines1 = self.lines[:]
        extralines = []
        __tracebackhide__ = True
        wnick = len(match_nickname) + 1
        started = False
        for line in lines2:
            nomatchprinted = False
            while lines1:
                nextline = lines1.pop(0)
                if line == nextline:
                    self._log("exact match:", repr(line))
                    started = True
                    break
                elif match_func(nextline, line):
                    self._log(f"{match_nickname}:", repr(line))
                    self._log("{:>{width}}".format("with:", width=wnick), repr(nextline))
                    started = True
                    break
                else:
                    if consecutive and started:
                        msg = f"no consecutive match: {line!r}"
                        self._log(msg)
                        self._log("{:>{width}}".format("with:", width=wnick), repr(nextline))
                        self._fail(msg)
                    if not nomatchprinted:
                        self._log("{:>{width}}".format("nomatch:", width=wnick), repr(line))
                        nomatchprinted = True
                    self._log("{:>{width}}".format("and:", width=wnick), repr(nextline))
                extralines.append(nextline)
            else:
                msg = f"remains unmatched: {line!r}"
                self._log(msg)
                self._fail(msg)
        self._log_output = []

    def no_fnmatch_line(self, pat):
        __tracebackhide__ = True
        import fnmatch

        self._no_match_line(pat, fnmatch.fnmatch, "fnmatch")

    def no_re_match_line(self, pat):
        __tracebackhide__ = True
        import re

        self._no_match_line(pat, lambda name, pat: bool(re.match(pat, name)), "re.match")

    def _no_match_line(self, pat, match_func, match_nickname):
        __tracebackhide__ = True
        nomatch_printed = False
        wnick = len(match_nickname) + 1
        for line in self.lines:
            if match_func(line, pat):
                msg = f"{match_nickname}: {pat!r}"
                self._log(msg)
                self._log("{:>{width}}".format("with:", width=wnick), repr(line))
                self._fail(msg)
            else:
                if not nomatch_printed:
                    self._log("{:>{width}}".format("nomatch:", width=wnick), repr(pat))
                    nomatch_printed = True
                self._log("{:>{width}}".format("and:", width=wnick), repr(line))
        self._log_output = []

    def _fail(self, msg):
        __tracebackhide__ = True
        log_text = self._log_text
        self._log_output = []
        fail(log_text)
