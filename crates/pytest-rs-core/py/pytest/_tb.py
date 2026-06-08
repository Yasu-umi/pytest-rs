"""pytest-style traceback formatting (--tb=long/short/line/native/no).

Long style shows the failing frame's function source with `>` on the
failing line and `E` prefixed exception lines, like pytest.
"""

import inspect
import linecache
import os
import traceback

# Set by the engine when terminal color is on; gates pygments highlighting
# and the red/bold markup below.
_color = False


def set_color(on):
    global _color
    _color = on


def _markup(text, *codes):
    if not _color:
        return text
    return "".join(f"\x1b[{c}m" for c in codes) + text + "\x1b[0m"


def validate_theme():
    """An error message when PYTEST_THEME / PYTEST_THEME_MODE is invalid
    (only checked when color is on), else None — pytest's startup check."""
    if not _color:
        return None
    theme = os.getenv("PYTEST_THEME")
    if theme is not None:
        try:
            from pygments.styles import get_style_by_name

            get_style_by_name(theme)
        except Exception:
            return (
                f"PYTEST_THEME environment variable has an invalid value: {theme!r}. "
                "Hint: See available pygments styles with `pygmentize -L styles`."
            )
    mode = os.getenv("PYTEST_THEME_MODE")
    if mode is not None and mode not in ("dark", "light"):
        return (
            f"PYTEST_THEME_MODE environment variable has an invalid value: {mode!r}. "
            "The allowed values are 'dark' (default) and 'light'."
        )
    return None


def _highlight(source):
    """pytest's TerminalWriter._highlight: pygments terminal colors with a
    leading reset, plain passthrough when color is off or pygments fails."""
    if not _color:
        return source
    try:
        from pygments import highlight as pygments_highlight
        from pygments.formatters.terminal import TerminalFormatter
        from pygments.lexers.python import PythonLexer

        mode = os.getenv("PYTEST_THEME_MODE", "dark")
        style = os.getenv("PYTEST_THEME")
        highlighted = pygments_highlight(
            source, PythonLexer(), TerminalFormatter(bg=mode, style=style)
        )
        if highlighted.endswith("\n") and not source.endswith("\n"):
            highlighted = highlighted[:-1]
        return "\x1b[0m" + highlighted
    except Exception:
        return source


def _visible_frames(exc):
    frames = []
    tb = exc.__traceback__
    while tb is not None:
        frame = tb.tb_frame
        hidden = frame.f_locals.get("__tracebackhide__") or frame.f_globals.get("__tracebackhide__")
        if not hidden:
            frames.append((frame, tb.tb_lineno))
        tb = tb.tb_next
    return frames


def _relpath(path):
    try:
        rel = os.path.relpath(path)
    except ValueError:
        return path
    return rel if not rel.startswith("..") else path


def _exception_lines(exc):
    # pytest's exconly: traceback.format_exception_only, which qualifies
    # non-builtin exception types with their module (upstream classes pin
    # __module__, e.g. "pytest.PytestUnraisableExceptionWarning").
    text = "".join(traceback.format_exception_only(type(exc), exc)).rstrip("\n")
    if (
        isinstance(exc, AssertionError)
        and str(exc).startswith("assert")
        # pytest's exconly(tryshort=True) quirk: the prefix is stripped only
        # when saferepr(exc) starts with "AssertionError('assert " — a quote
        # anywhere in the message flips repr to double quotes and the
        # "AssertionError: " stays.
        and repr(exc).startswith("AssertionError('assert ")
    ):
        # pytest's exconly(tryshort=True): only rewritten-assert explanations
        # drop the type name; raised AssertionErrors keep it.
        text = str(exc)
    return text.splitlines() or [type(exc).__name__]


def _source_block(frame, lineno):
    """The frame's source from its definition to the failing line, dedented
    to the definition's indentation (pytest), the whole block
    pygments-highlighted, '>' marking the failing line.

    Returns (lines, fail_indent): fail_indent is the displayed indentation
    of the failing line — pytest aligns the E lines under it."""
    code = frame.f_code
    try:
        source, start = inspect.getsourcelines(code)
    except (OSError, TypeError):
        source, start = [], None
    prefixes = []
    contents = []
    fail_indent = 4
    if start is not None and source:
        first = source[0]
        dedent = len(first) - len(first.lstrip())

        def strip_indent(raw):
            i = 0
            while i < dedent and i < len(raw) and raw[i] == " ":
                i += 1
            return raw[i:]

        for offset, raw in enumerate(source):
            current = start + offset
            if current > lineno:
                break
            content = strip_indent(raw.rstrip())
            if current == lineno:
                prefixes.append(">   ")
                fail_indent = len(content) - len(content.lstrip())
            else:
                prefixes.append("    ")
            contents.append(content)
    else:
        stripped = linecache.getline(code.co_filename, lineno).rstrip()
        if stripped:
            prefixes.append(">   ")
            contents.append(stripped.lstrip())
    highlighted = _highlight("\n".join(contents)).split("\n")
    lines = [f"{prefix}{line}" for prefix, line in zip(prefixes, highlighted)]
    return lines, fail_indent


def _location_line(code, lineno, suffix):
    """ "relpath:lineno: suffix" with the path bold red under color."""
    return f"{_markup(_relpath(code.co_filename), 1, 31)}:{lineno}: {suffix}"


def _format_short_frame(frame, lineno):
    code = frame.f_code
    source = linecache.getline(code.co_filename, lineno).strip()
    lines = [_location_line(code, lineno, f"in {code.co_name}").rstrip()]
    if source:
        lines.append(f"    {_highlight(source)}")
    return lines


def raise_location(exc):
    """ "relpath:lineno" of the last visible frame (where skip was raised)."""
    frames = _visible_frames(exc)
    if not frames:
        return None
    frame, lineno = frames[-1]
    return f"{_relpath(frame.f_code.co_filename)}:{lineno}"


def format_exception(exc, style="long"):
    if style == "no":
        return ""
    # pytest.fail(..., pytrace=False): no traceback, message only (with the
    # original exception's text when raised from an except block). --tb=line
    # still renders the one-line "path:lineno: Type: msg" form, so it falls
    # through to the line logic below.
    if not getattr(exc, "pytrace", True) and style != "line":
        parts = []
        context = exc.__context__
        if context is not None and not exc.__suppress_context__:
            parts.append(str(context))
            parts.append("")
            parts.append("During handling of the above exception, another exception occurred:")
        parts.append(str(getattr(exc, "msg", None) or ""))
        return "\n".join(parts)
    if style == "native":
        return "".join(traceback.format_exception(exc))
    # Exception groups render natively (upstream: the pytest-style frame
    # repr cannot show sub-exception trees). A non-group context exception
    # keeps its pytest-style block first, like upstream's chain repr.
    if isinstance(exc, BaseExceptionGroup):
        parts = []
        context = exc.__cause__ if exc.__cause__ is not None else exc.__context__
        if context is not None and not exc.__suppress_context__:
            parts.append(format_exception(context, style))
            parts.append("")
            if exc.__cause__ is not None:
                parts.append("The above exception was the direct cause of the following exception:")
            else:
                parts.append("During handling of the above exception, another exception occurred:")
            parts.append("")
        parts.append("".join(traceback.format_exception(exc, chain=False)).rstrip("\n"))
        return "\n".join(parts)

    frames = _visible_frames(exc)
    if not frames:
        return "".join(traceback.format_exception(exc))

    if style == "line":
        frame, lineno = frames[-1]
        message = _exception_lines(exc)[0]
        return f"{_relpath(frame.f_code.co_filename)}:{lineno}: {message}"

    lines = []
    if style == "short":
        for frame, lineno in frames:
            lines.extend(_format_short_frame(frame, lineno))
        for entry in _exception_lines(exc):
            lines.append(_markup(f"E   {entry}", 1, 31))
        return "\n".join(lines)

    # long (default): every frame shows its full source block with the
    # failing line marked, frames separated by the "_ _ _" rule; the last
    # frame carries the E lines (aligned under the failing line's indent,
    # like pytest) and the exception name.
    for index, (frame, lineno) in enumerate(frames):
        last = index == len(frames) - 1
        block, fail_indent = _source_block(frame, lineno)
        lines.extend(block)
        if last:
            e_prefix = "E" + " " * (3 + fail_indent)
            for entry in _exception_lines(exc):
                lines.append(_markup(f"{e_prefix}{entry}", 1, 31))
        lines.append("")
        suffix = type(exc).__name__ if last else ""
        lines.append(_location_line(frame.f_code, lineno, suffix))
        if not last:
            lines.append("_ " * 20)
            lines.append("")
    return "\n".join(lines)
