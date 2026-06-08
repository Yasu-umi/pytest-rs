import argparse
import enum
import os

from pytest import ExitCode, UsageError  # noqa: F401

_notset = object()


class Config:
    """Stub config type (mostly used for annotations upstream); instances
    built by _prepareconfig carry an option namespace for getoption()."""

    VERBOSITY_ASSERTIONS = "assertions"
    VERBOSITY_TEST_CASES = "test_cases"
    VERBOSITY_SUBTESTS = "subtests"

    class ArgsSource(enum.Enum):
        """Indicates the source of the test arguments (pytest's enum;
        the Rust-built Config proxy returns these members from
        ``config.args_source``)."""

        ARGS = enum.auto()
        INVOCATION_DIR = enum.auto()
        INCOVATION_DIR = INVOCATION_DIR  # backwards compatibility alias
        TESTPATHS = enum.auto()

    def __init__(self, option=None):
        self.option = option if option is not None else argparse.Namespace()

    def getoption(self, name, default=_notset, skip=False):
        name = name.lstrip("-").replace("-", "_")
        try:
            return getattr(self.option, name)
        except AttributeError:
            if default is not _notset:
                return default
            if skip:
                import pytest

                pytest.skip(f"no {name!r} option found")
            raise ValueError(f"no option named {name!r}") from None


from pytest._pluginmanager import PluginManager as PytestPluginManager  # noqa: E402, F401


def main(args=None, plugins=None):
    raise NotImplementedError("_pytest.config.main is not supported by pytest-rs")


def _prepareconfig(args=None, plugins=None):
    """Build a default-options Config. Upstream parses the full command
    line; here only the defaults consumed by ported helpers (e.g. the
    TerminalReporter summary-stats logic) are materialized."""
    if args:
        raise NotImplementedError(
            "_pytest.config._prepareconfig with args is not supported by pytest-rs"
        )
    option = argparse.Namespace(
        collectonly=False,
        verbose=0,
        quiet=0,
        capture="fd",
        setupshow=False,
        fold_skipped=True,
    )
    return Config(option)


def parse_warning_filter(arg, *, escape):
    """Parse a warnings filter string (the engine's own parser, which is
    already a port of upstream's)."""
    from pytest import _wcapture

    return _wcapture.parse_filter(arg, escape=escape)


def filename_arg(path, optname):
    """Argparse type validator rejecting directories."""
    if os.path.isdir(path):
        raise UsageError(f"{optname} must be a filename, given: {path}")
    return path


def _iter_rewritable_modules(package_files):
    """Given an iterable of file names in a source distribution, return the
    "names" that should be marked for assertion rewrite (handles dist-info
    and egg/src layouts; see pytest-mock#167)."""
    package_files = list(package_files)
    seen_some = False
    for fn in package_files:
        is_simple_module = "/" not in fn and fn.endswith(".py")
        is_package = fn.count("/") == 1 and fn.endswith("__init__.py")
        if is_simple_module:
            module_name, _ = os.path.splitext(fn)
            # we ignore "setup.py" at the root of the distribution as well
            # as editable installation finder modules made by setuptools
            if module_name != "setup" and not module_name.startswith("__editable__"):
                seen_some = True
                yield module_name
        elif is_package:
            package_name = os.path.dirname(fn)
            seen_some = True
            yield package_name

    if not seen_some:
        # No packages or modules found: retry with the first path component
        # stripped ("src" based source trees).
        new_package_files = []
        for fn in package_files:
            parts = fn.split("/")
            new_fn = "/".join(parts[1:])
            if new_fn:
                new_package_files.append(new_fn)
        if new_package_files:
            yield from _iter_rewritable_modules(new_package_files)


from _pytest._stub import __getattr__  # noqa: E402, F401
