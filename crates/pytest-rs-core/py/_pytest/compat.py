import enum
import inspect
import os


def assert_never(value):
    raise AssertionError(f"Unhandled value: {value!r}")


def safe_getattr(object, name, default):
    try:
        return getattr(object, name, default)
    except Exception:
        return default


def safe_isclass(obj):
    try:
        return inspect.isclass(obj)
    except Exception:
        return False


def get_real_func(obj):
    while hasattr(obj, "__wrapped__"):
        obj = obj.__wrapped__
    return obj


def running_on_ci():
    return os.environ.get("CI", "").lower() in ("true", "1") or "BUILD_NUMBER" in os.environ


class NotSetType(enum.Enum):
    token = 0


NOTSET = NotSetType.token
LEGACY_PATH = None  # py.path.local is not supported
