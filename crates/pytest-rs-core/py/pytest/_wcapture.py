"""Session-wide warning capture.

Warnings that pass the active filters reach our showwarning hook and are
recorded for the warnings summary and the "N warnings" count, mirroring
pytest's warning capture.
"""

import os
import re
import sys
import warnings
from collections import Counter

captured: list[dict[str, object]] = []
current_test: str | None = None


def set_current_test(nodeid):
    global current_test
    current_test = nodeid


def _showwarning(message, category, filename, lineno, file=None, line=None):
    captured.append(
        {
            "message": str(message),
            "category": category,
            "filename": filename,
            "lineno": lineno,
            "test": current_test,
        }
    )


_original_showwarning = None


def install():
    # Mirror _pytest.warnings.catch_warnings_for_item's session filters on
    # top of the interpreter defaults (no reset, so -W/PYTHONWARNINGS from
    # the outside keep working).
    global _original_showwarning
    if not sys.warnoptions:
        # If the user is not explicitly configuring warning filters, show
        # deprecation warnings by default (#2908).
        warnings.filterwarnings("always", category=DeprecationWarning)
        warnings.filterwarnings("always", category=PendingDeprecationWarning)
    from pytest._warning_types import PytestRemovedIn9Warning

    warnings.filterwarnings("error", category=PytestRemovedIn9Warning)
    _original_showwarning = warnings.showwarning
    warnings.showwarning = _showwarning


def uninstall():
    """Stop capturing: warnings emitted after the summary (e.g. the
    unraisable session cleanup) print through the original showwarning."""
    global _original_showwarning
    if _original_showwarning is not None:
        warnings.showwarning = _original_showwarning
        _original_showwarning = None


def _resolve_category(category):
    """pytest's _resolve_warning_category: like warnings._getcategory but
    lets ImportErrors propagate."""
    if not category:
        return Warning
    if "." not in category:
        import builtins as m

        klass = category
    else:
        module, _, klass = category.rpartition(".")
        m = __import__(module, None, None, [klass])
    cat = getattr(m, klass)
    if not issubclass(cat, Warning):
        import pytest

        raise pytest.UsageError(f"{cat} is not a Warning subclass")
    return cat


def parse_filter(arg, escape):
    """pytest's parse_warning_filter: warnings._setoption parsing with
    optional escaping and UsageError-friendly messages."""
    import pytest

    error_template = (
        "while parsing the following warning configuration:\n\n"
        f"  {arg}\n\n"
        "This error occurred:\n\n" + "{error}\n"
    )

    parts = arg.split(":")
    if len(parts) > 5:
        doc_url = "https://docs.python.org/3/library/warnings.html#describing-warning-filters"
        error = (
            f"Too many fields ({len(parts)}), expected at most 5 separated by colons:\n\n"
            "  action:message:category:module:line\n\n"
            f"For more information please consult: {doc_url}\n"
        )
        raise pytest.UsageError(error_template.format(error=error))
    while len(parts) < 5:
        parts.append("")
    action_, message, category_, module, lineno_ = (s.strip() for s in parts)
    try:
        action = warnings._getaction(action_)
    except warnings._OptionError as e:
        raise pytest.UsageError(error_template.format(error=str(e))) from None
    try:
        category = _resolve_category(category_)
    except ImportError:
        raise
    except Exception as e:
        raise pytest.UsageError(error_template.format(error=f"{type(e).__name__}: {e}")) from None
    if message and escape:
        message = re.escape(message)
    if module and escape:
        module = re.escape(module) + r"\Z"
    if lineno_:
        try:
            lineno = int(lineno_)
            if lineno < 0:
                raise ValueError("number is negative")
        except ValueError as e:
            raise pytest.UsageError(
                error_template.format(error=f"invalid lineno {lineno_!r}: {e}")
            ) from None
    else:
        lineno = 0
    try:
        re.compile(message)
        re.compile(module)
    except re.error as e:
        raise pytest.UsageError(
            error_template.format(error=f"Invalid regex {e.pattern!r}: {e}")
        ) from None
    return action, message, category, module, lineno


def _apply_filter(spec, escape):
    from pytest._warning_types import PytestConfigWarning

    try:
        warnings.filterwarnings(*parse_filter(spec, escape=escape))
    except ImportError as e:
        warnings.warn(f"Failed to import filter module '{e.name}': {spec}", PytestConfigWarning)


# The session's filter specs (ini then -W), kept for pytester: upstream's
# in-process nested runs inherit the outer session's warning filters.
session_specs: list = []


def apply_session_filters(ini_specs, w_specs):
    """Apply the `filterwarnings` ini lines (unescaped regexes), then -W
    specs (escaped, python -W semantics) so the command line wins. A parent
    pytester's forwarded filterwarnings marks apply first — lowest priority,
    like upstream's in-process nesting."""
    import os

    forwarded = [
        spec
        for spec in os.environ.get("PYTEST_RS_FORWARDED_FILTERS", "").split("\n")
        if spec.strip()
    ]
    session_specs[:] = [*forwarded, *ini_specs, *w_specs]
    for spec in forwarded:
        _apply_filter(spec, escape=False)
    for spec in ini_specs:
        _apply_filter(spec, escape=False)
    for spec in w_specs:
        _apply_filter(spec, escape=True)


def begin_item_filters(specs):
    """Enter a catch_warnings block applying @pytest.mark.filterwarnings
    specs on top of the session filters (farthest mark first, so the
    closest mark wins). Entered for every item — like pytest — so the
    "default" action's once-per-location registry resets per test."""
    ctx = warnings.catch_warnings()
    ctx.__enter__()
    try:
        for spec in specs:
            warnings.filterwarnings(*parse_filter(spec, escape=False))
    except BaseException:
        ctx.__exit__(None, None, None)
        raise
    return ctx


def end_item_filters(ctx):
    ctx.__exit__(None, None, None)


def count():
    return len(captured)


def _location(warning):
    """Nodeid if the warning was raised during a test, else the warning's
    origin as an invocation-dir-relative file:lineno (pytest's
    WarningReport.get_location)."""
    if warning["test"]:
        return warning["test"]
    filename = warning["filename"]
    try:
        rel = os.path.relpath(filename)
        if not rel.startswith(".."):
            filename = rel
    except ValueError:
        pass
    return f"{filename}:{warning['lineno']}"


def summary_lines():
    """Warnings grouped by formatted message with their locations above,
    the way pytest's summary_warnings renders them."""
    grouped: dict[str, list[str]] = {}
    for warning in captured:
        message = warnings.formatwarning(
            warning["message"],
            warning["category"],
            warning["filename"],
            warning["lineno"],
        )
        grouped.setdefault(message, []).append(_location(warning))
    lines = []
    for message, locations in grouped.items():
        if locations:
            if len(locations) < 10:
                lines.extend(locations)
            else:
                counts = Counter(loc.split("::", 1)[0] for loc in locations)
                lines.extend(
                    f"{filename}: {n} warning{'s' if n != 1 else ''}"
                    for filename, n in counts.items()
                )
            message = "\n".join("  " + line for line in message.splitlines())
        lines.extend(message.rstrip().splitlines())
        lines.append("")
    return lines
