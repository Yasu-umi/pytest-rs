import os

from pytest import ExitCode  # noqa: F401

from _pytest._stub import __getattr__  # noqa: E402, F401


class Session:
    """Stub session type (mostly used for annotations upstream)."""


def _in_venv(path) -> bool:
    """Is this path the root of a virtual environment? (pyvenv.cfg check)"""
    try:
        return os.path.isfile(os.path.join(str(path), "pyvenv.cfg"))
    except OSError:
        return False
