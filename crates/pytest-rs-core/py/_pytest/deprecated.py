"""Deprecation messages (upstream _pytest/deprecated.py port)."""

from warnings import warn

from _pytest.warning_types import (
    PytestDeprecationWarning,
    PytestRemovedIn9Warning,
    PytestRemovedIn10Warning,
    UnformattedWarning,
)

# set of plugins which have been integrated into the core; we use this list
# to ignore them during registration to avoid conflicts
DEPRECATED_EXTERNAL_PLUGINS = {
    "pytest_catchlog",
    "pytest_capturelog",
    "pytest_faulthandler",
    "pytest_subtests",
}

YIELD_FIXTURE = PytestDeprecationWarning(
    "@pytest.yield_fixture is deprecated.\nUse @pytest.fixture instead; they are the same."
)

# This deprecation is never really meant to be removed.
PRIVATE = PytestDeprecationWarning("A private pytest class or function was used.")

HOOK_LEGACY_PATH_ARG = UnformattedWarning(
    PytestRemovedIn9Warning,
    "The ({pylib_path_arg}: py.path.local) argument is deprecated, "
    "please use ({pathlib_path_arg}: pathlib.Path)\n"
    "see https://docs.pytest.org/en/latest/deprecations.html"
    "#py-path-local-arguments-for-hooks-replaced-with-pathlib-path",
)

NODE_CTOR_FSPATH_ARG = UnformattedWarning(
    PytestRemovedIn9Warning,
    "The (fspath: py.path.local) argument to {node_type_name} is deprecated. "
    "Please use the (path: pathlib.Path) argument instead.\n"
    "See https://docs.pytest.org/en/latest/deprecations.html"
    "#fspath-argument-for-node-constructors-replaced-with-pathlib-path",
)

HOOK_LEGACY_MARKING = UnformattedWarning(
    PytestDeprecationWarning,
    "The hook{type} {fullname} uses old-style configuration options (marks or attributes).\n"
    "Please use the pytest.hook{type}({hook_opts}) decorator instead\n"
    " to configure the hooks.\n"
    " See https://docs.pytest.org/en/latest/deprecations.html"
    "#configuring-hook-specs-impls-using-markers",
)

MARKED_FIXTURE = PytestRemovedIn9Warning(
    "Marks applied to fixtures have no effect\n"
    "See docs: https://docs.pytest.org/en/stable/deprecations.html"
    "#applying-a-mark-to-a-fixture-function"
)

MONKEYPATCH_LEGACY_NAMESPACE_PACKAGES = PytestRemovedIn10Warning(
    "monkeypatch.syspath_prepend() called with pkg_resources legacy namespace "
    "packages detected.\n"
    "Legacy namespace packages (using pkg_resources.declare_namespace) are deprecated.\n"
    "Please use native namespace packages (PEP 420) instead.\n"
    "See https://docs.pytest.org/en/stable/deprecations.html#monkeypatch-fixup-namespace-packages"
)


def check_ispytest(ispytest: bool) -> None:
    if not ispytest:
        warn(PRIVATE, stacklevel=3)


from _pytest._stub import __getattr__  # noqa: E402, F401
