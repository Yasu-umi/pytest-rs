"""Fallback for _pytest internals that pytest-rs does not implement.

Imported names resolve to a callable that raises on use, so upstream test
files import successfully; only tests that exercise the internal fail.
"""


class _Unsupported:
    # Attributes the pytest-rs engine probes during collection; answering
    # them would make stubs look like fixtures/marks.
    _OPAQUE = ("_pytestfixturefunction", "pytestmark", "mark", "name")

    def __init__(self, name):
        self._name = name

    def __call__(self, *args, **kwargs):
        raise NotImplementedError(f"_pytest internal {self._name!r} is not supported by pytest-rs")

    def __getattr__(self, attr):
        if attr.startswith("__") or attr in _Unsupported._OPAQUE:
            raise AttributeError(attr)
        return _Unsupported(f"{self._name}.{attr}")

    def __mro_entries__(self, bases):
        # Allow `class X(SomeInternal):` to at least be defined.
        return (object,)


def __getattr__(name):
    if name.startswith("__"):
        raise AttributeError(name)
    return _Unsupported(name)
