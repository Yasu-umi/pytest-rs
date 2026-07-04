import argparse
import collections.abc
import enum
import os
import types

from pytest import ExitCode, UsageError  # noqa: F401

_notset = object()


class ConftestImportFailure(Exception):
    """Raised when importing a conftest.py fails (upstream parity)."""

    def __init__(self, path, *, cause):
        self.path = path
        self.cause = cause

    def __str__(self):
        return f"{type(self.cause).__name__}: {self.cause} (from {self.path})"


class Config:
    """Stub config type (mostly used for annotations upstream); instances
    built by _prepareconfig carry an option namespace for getoption()."""

    VERBOSITY_ASSERTIONS = "assertions"
    VERBOSITY_TEST_CASES = "test_cases"
    VERBOSITY_SUBTESTS = "subtests"
    _VERBOSITY_INI_DEFAULT = "auto"

    @staticmethod
    def _verbosity_ini_name(verbosity_type):
        return f"verbosity_{verbosity_type}"

    @staticmethod
    def _add_verbosity_ini(parser, verbosity_type, help):
        """Register a fine-grained verbosity ini (pytest's helper). Plugins
        call this from pytest_addoption; config.get_verbosity reads it back."""
        parser.addini(
            Config._verbosity_ini_name(verbosity_type),
            help=help,
            type="string",
            default=Config._VERBOSITY_INI_DEFAULT,
        )

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

    @classmethod
    def fromdictargs(cls, option_dict, args):
        from _pytest.config import _native_prepareconfig

        # inifilename steers ini discovery itself (rootdir/inipath), so it
        # must reach the native parser as a real "-c" CLI arg, not just be
        # set on config.option after the fact (upstream applies option_dict
        # to config.option before parsing, for the same reason).
        native_args = list(args)
        inifilename = option_dict.get("inifilename")
        if inifilename is not None:
            native_args = ["-c", inifilename, *native_args]
        config = _native_prepareconfig(native_args)
        config._mark_as_parsed()
        for key, value in option_dict.items():
            setattr(config.option, key, value)
        return config

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


def get_plugin_manager() -> "PytestPluginManager":
    """Return the global plugin manager (backward-compat API, upstream #787)."""
    from pytest._pluginmanager import pluginmanager

    return pluginmanager


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


def _strtobool(val):
    """Convert a string representation of truth to True or False.

    True values are 'y', 'yes', 't', 'true', 'on', and '1'; false values
    are 'n', 'no', 'f', 'false', 'off', and '0'. Raises ValueError if
    'val' is anything else (copied from distutils.util)."""
    val = val.lower()
    if val in ("y", "yes", "t", "true", "on", "1"):
        return True
    elif val in ("n", "no", "f", "false", "off", "0"):
        return False
    else:
        raise ValueError(f"invalid truth value {val!r}")


def _get_plugin_specs_as_list(specs):
    """Parse a plugins specification into a list of plugin names."""
    # None means empty.
    if specs is None:
        return []
    # Workaround for #3899 - a submodule called "pytest_plugins".
    if isinstance(specs, types.ModuleType):
        return []
    # Comma-separated list.
    if isinstance(specs, str):
        return specs.split(",") if specs else []
    # Direct specification.
    if isinstance(specs, collections.abc.Sequence):
        return list(specs)
    raise UsageError(
        "Plugins may be specified as a sequence or a ','-separated string of "
        f"plugin names. Got: {specs!r}"
    )


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
