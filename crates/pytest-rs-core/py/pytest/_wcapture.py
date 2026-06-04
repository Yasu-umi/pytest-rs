"""Session-wide warning capture.

Warnings that pass the active filters reach our showwarning hook and are
recorded for the warnings summary and the "N warnings" count, mirroring
pytest's warning capture.
"""

import warnings

captured: list[dict[str, object]] = []


def _showwarning(message, category, filename, lineno, file=None, line=None):
    captured.append(
        {
            "message": str(message),
            "category": category.__name__,
            "filename": filename,
            "lineno": lineno,
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
    lines = []
    for warning in captured:
        location = f"{warning['filename']}:{warning['lineno']}"
        lines.append(f"{location}: {warning['category']}: {warning['message']}")
    return lines
