"""Support for freezing a test suite into an executable (cx_freeze & co.)."""

from _pytest._stub import __getattr__  # noqa: E402, F401


def freeze_includes():
    """The module names a frozen binary has to bundle for pytest to work --
    upstream's helper, which walks the `_pytest` package. pytest-rs's engine is
    a binary that extracts its own shims, so this only matters for the shim
    package a frozen *test suite* imports."""
    import _pytest

    return list(_iter_all_modules(_pytest))


def _iter_all_modules(package, prefix=""):
    """The dotted names of every module below `package` (upstream's
    freeze_support._iter_all_modules)."""
    import os
    import pkgutil

    if isinstance(package, str):
        path = package
    else:
        path = os.path.dirname(next(iter(package.__path__)) + os.sep)
        prefix = package.__name__ + "."
    for _, name, is_package in pkgutil.iter_modules([path]):
        if is_package:
            yield from _iter_all_modules(os.path.join(path, name), prefix=f"{prefix}{name}.")
        else:
            yield prefix + name
