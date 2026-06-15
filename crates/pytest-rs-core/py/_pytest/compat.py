import enum
import functools
import inspect
import os
import sys
from inspect import Parameter, signature


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


_non_printable_ascii_translate_table = {
    i: f"\\x{i:02x}" for i in range(128) if i not in range(32, 127)
}
_non_printable_ascii_translate_table.update({ord("\t"): "\\t", ord("\r"): "\\r", ord("\n"): "\\n"})


def ascii_escaped(val):
    r"""If val is pure ASCII, return it as an str, otherwise, escape bytes
    objects into a sequence of escaped bytes, and strings into a sequence of
    escaped unicode ids (upstream _pytest.compat.ascii_escaped)."""
    if isinstance(val, bytes):
        ret = val.decode("ascii", "backslashreplace")
    else:
        ret = val.encode("unicode_escape").decode("ascii")
    return ret.translate(_non_printable_ascii_translate_table)


def num_mock_patch_args(function) -> int:
    """Return number of arguments used up by mock arguments (if any)."""
    patchings = getattr(function, "patchings", None)
    if not patchings:
        return 0

    mock_sentinel = getattr(sys.modules.get("mock"), "DEFAULT", object())
    ut_mock_sentinel = getattr(sys.modules.get("unittest.mock"), "DEFAULT", object())

    return len(
        [
            p
            for p in patchings
            if not p.attribute_name and (p.new is mock_sentinel or p.new is ut_mock_sentinel)
        ]
    )


def getfuncargnames(function, *, name: str = "", cls: type | None = None):
    """Return the names of a function's mandatory arguments, excluding those
    bound to an instance/type, with defaults, or replaced by mocks (faithful
    port of _pytest.compat.getfuncargnames)."""
    try:
        parameters = signature(function).parameters.values()
    except (ValueError, TypeError) as e:
        from _pytest.outcomes import fail

        fail(f"Could not determine arguments of {function!r}: {e}", pytrace=False)

    arg_names = tuple(
        p.name
        for p in parameters
        if (p.kind is Parameter.POSITIONAL_OR_KEYWORD or p.kind is Parameter.KEYWORD_ONLY)
        and p.default is Parameter.empty
    )
    if not name:
        name = function.__name__

    if not any(p.kind is Parameter.POSITIONAL_ONLY for p in parameters) and (
        cls and not isinstance(inspect.getattr_static(cls, name, default=None), staticmethod)
    ):
        arg_names = arg_names[1:]
    if hasattr(function, "__wrapped__"):
        arg_names = arg_names[num_mock_patch_args(function) :]
    return arg_names


def get_user_id():
    """Return the current process's real user id or None if it could not be
    determined (upstream get_user_id)."""
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
