"""TerminalWriter: trimmed port of upstream's _pytest._io.terminalwriter
(markup, separator lines, current-line width tracking). Used by the
get_terminal_writer() escape hatch (pytest-timeout's dumps) and as the
TerminalReporter shim's `_tw`."""

import os
import shutil
import sys


def get_terminal_width():
    width, _ = shutil.get_terminal_size(fallback=(80, 24))
    # The Windows get_terminal_size may be bogus, let's sanify a bit.
    if width < 40:
        width = 80
    return width


def should_do_markup(file):
    if os.environ.get("PY_COLORS") == "1":
        return True
    if os.environ.get("PY_COLORS") == "0":
        return False
    if os.environ.get("NO_COLOR"):
        return False
    if os.environ.get("FORCE_COLOR"):
        return True
    return hasattr(file, "isatty") and file.isatty() and os.environ.get("TERM") != "dumb"


class TerminalWriter:
    _esctable = dict(
        black=30,
        red=31,
        green=32,
        yellow=33,
        blue=34,
        purple=35,
        cyan=36,
        white=37,
        Black=40,
        Red=41,
        Green=42,
        Yellow=43,
        Blue=44,
        Purple=45,
        Cyan=46,
        White=47,
        bold=1,
        light=2,
        blink=5,
        invert=7,
    )

    def __init__(self, file=None):
        self._file = file or sys.stdout
        self.hasmarkup = should_do_markup(self._file)
        self.code_highlight = False
        self._current_line = ""
        self._fullwidth = None

    @property
    def fullwidth(self):
        if self._fullwidth is not None:
            return self._fullwidth
        return get_terminal_width()

    @fullwidth.setter
    def fullwidth(self, value):
        self._fullwidth = value

    @property
    def width_of_current_line(self):
        return len(self._current_line)

    def markup(self, text, **markup):
        if self.hasmarkup:
            esc = [
                self._esctable[name] for name, on in markup.items() if on and name in self._esctable
            ]
            if esc:
                text = "".join(f"\x1b[{cod}m" for cod in esc) + text + "\x1b[0m"
        return text

    def write(self, msg, *, flush=False, **markup):
        if not msg:
            return
        current_line = msg.rsplit("\n", 1)[-1]
        if "\n" in msg:
            self._current_line = current_line
        else:
            self._current_line += current_line
        self._file.write(self.markup(msg, **markup))
        if flush:
            self.flush()

    def write_raw(self, content, *, flush=False):
        self._file.write(content)
        if flush:
            self.flush()

    def line(self, line="", **markup):
        self.write(line, **markup)
        self.write("\n")

    def sep(self, sepchar, title=None, fullwidth=None, **markup):
        if fullwidth is None:
            fullwidth = self.fullwidth
        if sys.platform == "win32":
            # The Windows shell prints an extra empty line when the line
            # ends on the last column.
            fullwidth -= 1
        if title is not None:
            # We want 2 + 2*len(fill) + len(title) <= fullwidth.
            n = max((fullwidth - len(title) - 2) // (2 * len(sepchar)), 1)
            fill = sepchar * n
            line = f"{fill} {title} {fill}"
        else:
            line = sepchar * (fullwidth // len(sepchar))
        if len(line) + len(sepchar.rstrip()) <= fullwidth:
            line += sepchar.rstrip()
        self.line(line, **markup)

    def flush(self):
        self._file.flush()


from _pytest._stub import __getattr__  # noqa: E402, F401
