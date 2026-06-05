"""pytest-style traceback formatting (--tb=long/short/line/native/no).

Long style shows the failing frame's function source with `>` on the
failing line and `E` prefixed exception lines, like pytest.
"""

import inspect
import linecache
import os
import traceback


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
    text = f"{type(exc).__name__}: {exc}" if str(exc) else type(exc).__name__
    if isinstance(exc, AssertionError) and str(exc).startswith("assert"):
        # pytest's exconly(tryshort=True): only rewritten-assert explanations
        # drop the type name; raised AssertionErrors keep it.
        text = str(exc)
    return text.splitlines() or [type(exc).__name__]


def _format_last_frame(frame, lineno, exc):
    lines = []
    code = frame.f_code
    try:
        source, start = inspect.getsourcelines(code)
    except (OSError, TypeError):
        source, start = [], None
    if start is not None:
        for offset, raw in enumerate(source):
            current = start + offset
            if current > lineno:
                break
            prefix = ">   " if current == lineno else "    "
            lines.append(f"{prefix}{raw.rstrip()}")
    else:
        stripped = linecache.getline(code.co_filename, lineno).rstrip()
        if stripped:
            lines.append(f">   {stripped}")
    for entry in _exception_lines(exc):
        lines.append(f"E       {entry}")
    lines.append("")
    lines.append(f"{_relpath(code.co_filename)}:{lineno}: {type(exc).__name__}")
    return lines


def _format_short_frame(frame, lineno):
    code = frame.f_code
    source = linecache.getline(code.co_filename, lineno).strip()
    lines = [f"{_relpath(code.co_filename)}:{lineno}: in {code.co_name}"]
    if source:
        lines.append(f"    {source}")
    return lines


def format_exception(exc, style="long"):
    if style == "no":
        return ""
    if style == "native":
        return "".join(traceback.format_exception(exc))

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
            lines.append(f"E       {entry}")
        return "\n".join(lines)

    # long (default): short entries for outer frames, full source for the
    # failing frame.
    for frame, lineno in frames[:-1]:
        lines.extend(_format_short_frame(frame, lineno))
    if len(frames) > 1:
        lines.append("_ " * 20)
    frame, lineno = frames[-1]
    lines.extend(_format_last_frame(frame, lineno, exc))
    return "\n".join(lines)
