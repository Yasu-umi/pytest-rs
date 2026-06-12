import enum
import functools
import inspect
import os


def assert_never(value):
    raise AssertionError(f"Unhandled value: {value!r}")


def safe_getattr(object, name, default):
    from _pytest.outcomes import OutcomeException
    try:
        return getattr(object, name, default)
    except (OutcomeException, Exception):
        return default


def safe_isclass(obj):
    try:
        return inspect.isclass(obj)
    except Exception:
        return False


def get_real_func(obj):
    obj = inspect.unwrap(obj)
    if isinstance(obj, functools.partial):
        obj = obj.func
    return obj


def running_on_ci():
    return os.environ.get("CI", "").lower() in ("true", "1") or "BUILD_NUMBER" in os.environ


class NotSetType(enum.Enum):
    token = 0


NOTSET = NotSetType.token


def legacy_path(path):
    """A py.path.local-alike for the given path (the LocalPath shim)."""
    from pytest._tmp_path import LocalPath

    return LocalPath(path)


LEGACY_PATH = None  # py.path.local itself is not bundled


def get_user_id():
    """Return the current process's real user id or None if it could not be
    determined (upstream get_user_id)."""
    import sys

    if sys.platform == "win32" or sys.platform == "emscripten":
        # win32 does not have a getuid() function;
        # Emscripten has a return 0 stub.
        return None
    # On other platforms, a return value of -1 is assumed to indicate that
    # the current process's real user id could not be determined.
    erruid = -1
    uid = os.getuid()
    return uid if uid != erruid else None


from _pytest._stub import __getattr__  # noqa: E402, F401
