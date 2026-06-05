"""Session-wide warning capture.

Warnings that pass the active filters reach our showwarning hook and are
recorded for the warnings summary and the "N warnings" count, mirroring
pytest's warning capture.
"""

import warnings

captured: list[dict[str, object]] = []
current_test: str | None = None


def set_current_test(nodeid):
    global current_test
    current_test = nodeid


def _showwarning(message, category, filename, lineno, file=None, line=None):
    captured.append(
        {
            "message": str(message),
            "category": category.__name__,
            "filename": filename,
            "lineno": lineno,
            "test": current_test,
        }
    )


def install():
    # pytest's default filters: show everything once per location, and
    # always show (Pending)DeprecationWarning.
    warnings.simplefilter("default")
    warnings.filterwarnings("always", category=DeprecationWarning)
    warnings.filterwarnings("always", category=PendingDeprecationWarning)
    warnings.showwarning = _showwarning


def count():
    return len(captured)


def summary_lines():
    """Warnings grouped under the test nodeid they were emitted in, the way
    pytest's warnings summary does."""
    lines = []
    last_test = None
    for warning in captured:
        test = warning.get("test")
        if test and test != last_test:
            lines.append(test)
        last_test = test
        location = f"{warning['filename']}:{warning['lineno']}"
        indent = "  " if test else ""
        lines.append(f"{indent}{location}: {warning['category']}: {warning['message']}")
    return lines
