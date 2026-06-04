import shutil
import sys


class TerminalWriter:
    def __init__(self, file=None):
        self._file = file or sys.stdout
        self.hasmarkup = False
        self.code_highlight = False

    @property
    def fullwidth(self):
        return shutil.get_terminal_size().columns

    def write(self, msg, **markup):
        self._file.write(msg)

    def line(self, line="", **markup):
        self._file.write(line + "\n")

    def sep(self, sepchar, title=None, fullwidth=None, **markup):
        width = fullwidth or self.fullwidth
        if title is not None:
            body = f" {title} "
            fill = max(0, (width - len(body)) // (2 * len(sepchar)))
            line = sepchar * fill + body + sepchar * fill
        else:
            line = sepchar * (width // len(sepchar))
        self.line(line)

    def flush(self):
        self._file.flush()

    def markup(self, text, **markup):
        return text


from _pytest._stub import __getattr__  # noqa: E402, F401
