"""Fallback for _pytest internals that pytest-rs does not implement.

Imported names resolve to a callable that raises on use, so upstream test
files import successfully; only tests that exercise the internal fail.
"""


class _Unsupported:
    def __init__(self, name):
        self._name = name

    def __call__(self, *args, **kwargs):
        raise NotImplementedError(f"_pytest internal {self._name!r} is not supported by pytest-rs")

    def __getattr__(self, attr):
        if attr.startswith("__"):
            raise AttributeError(attr)
        return _Unsupported(f"{self._name}.{attr}")

    def __mro_entries__(self, bases):
        # Allow `class X(SomeInternal):` to at least be defined.
        return (object,)


def __getattr__(name):
    if name.startswith("__"):
        raise AttributeError(name)
    return _Unsupported(name)
