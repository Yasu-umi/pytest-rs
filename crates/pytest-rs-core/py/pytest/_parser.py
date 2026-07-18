"""Parser shim for plugin/conftest pytest_addoption hooks: records option
and ini specs so config.getoption()/getini() can fall back to the
plugin-declared defaults (e.g. pytest-timeout's `timeout_func_only`
bool ini defaulting to False). CLI parsing of plugin-defined flags is not
wired up yet — unknown flags still error at the Rust argument parser."""

from __future__ import annotations

import argparse
import shlex
import textwrap
from pathlib import Path
from typing import Any

# Sentinel for "no default passed to addini" — distinct from an explicit
# default=None (which makes getini return None regardless of type).
_UNSET = object()

# name -> {"type": str | None, "default": Any, "aliases": list[str]}
ini_specs: dict[str, dict[str, Any]] = {}
# alias -> canonical ini name
ini_aliases: dict[str, str] = {}
# dest -> {"default": Any, "type": callable | None, "action": str | None}
option_specs: dict[str, dict[str, Any]] = {}
# "--flag" -> dest, for deferred CLI token resolution
flag_dests: dict[str, str] = {}
# "--flag" -> action, per-flag (two flags can share a dest but have different
# actions, e.g. --nomigrations=store_true / --migrations=store_false).
flag_actions: dict[str, str | None] = {}
# group name -> OptionGroup, so repeated parser.getgroup(name) calls (e.g.
# from multiple pytest_addoption hookimpls) accumulate onto the same group
# instead of each getting an independent, display-invisible copy.
option_groups: dict[str, OptionGroup] = {}

# -h/--help is a Rust-native clap flag (Config::build_clap_command), never
# registered via Parser.addoption — but plugins like pytest-django read
# options.help off Parser.parse_known_args(args) during
# pytest_load_initial_conftests to skip their own early setup, so it still
# needs an entry here for that reflection to see it.
option_specs["help"] = {
    "default": False,
    "type": None,
    "action": "store_true",
    "nargs": None,
    "choices": None,
}
flag_dests["--help"] = "help"
flag_dests["-h"] = "help"
flag_actions["--help"] = "store_true"
flag_actions["-h"] = "store_true"


class Option:
    """Minimal Option wrapper for plugin compatibility.
    Exposes names() and attrs() so plugins like typeguard can introspect
    the last registered option (e.g. group.options[-1].names()[0])."""

    def __init__(self, opts: tuple[str, ...], attrs: dict[str, Any]) -> None:
        self._opts = opts
        self._attrs = attrs

    def names(self) -> tuple[str, ...]:
        return self._opts

    def attrs(self) -> dict[str, Any]:
        return self._attrs


class OptionGroup:
    def __init__(self, parser: Parser, name: str = "", description: str = "") -> None:
        self.parser = parser
        self.name = name
        self.description = description
        self.options: list[Option] = []

    def addoption(self, *opts: str, **attrs: Any) -> None:
        dest = attrs.get("dest")
        if dest is None:
            for opt in opts:
                if opt.startswith("--"):
                    dest = opt.lstrip("-").replace("-", "_")
                    break
        if dest is None:
            return
        default = attrs.get("default")
        if default is None and attrs.get("action") == "store_true":
            default = False
        option_specs[dest] = {
            "default": default,
            "type": attrs.get("type"),
            "action": attrs.get("action"),
            "nargs": attrs.get("nargs"),
            "choices": attrs.get("choices"),
        }
        action = attrs.get("action")
        for opt in opts:
            if opt.startswith("-"):
                flag_dests[opt] = dest
                flag_actions[opt] = action
        self.options.append(Option(opts, attrs))

    _addoption = addoption

    def addini(self, *args: Any, **kwargs: Any) -> None:
        self.parser.addini(*args, **kwargs)


class Parser:
    def getgroup(self, name: str, description: str = "", after: str | None = None) -> OptionGroup:
        existing = option_groups.get(name)
        if existing is not None:
            return existing
        group = OptionGroup(self, name, description)
        option_groups[name] = group
        return group

    def addoption(self, *opts: str, **attrs: Any) -> None:
        # Routed through the "" (ungrouped) bucket in option_groups, same as
        # a named getgroup(), so the Option (with its help text) survives
        # for --help rendering instead of living only on a throwaway group.
        self.getgroup("").addoption(*opts, **attrs)

    def parse_known_args(self, args, namespace=None):
        """argparse-style early parse: a namespace carrying the registered
        option defaults with the CLI `args` applied (pytest-django reads
        options.ds/.dc/.itv/.version/.help here). Unknown tokens are ignored."""
        ns = namespace if namespace is not None else OptionNamespace()
        apply_cli_args(ns, [str(a) for a in args])
        return ns

    def parse_known_and_unknown_args(self, args, namespace=None):
        ns = namespace if namespace is not None else OptionNamespace()
        unknown = apply_cli_args(ns, [str(a) for a in args])
        return ns, unknown

    def addini(
        self,
        name: str,
        help: str | None = None,
        type: str | None = None,
        default: Any = _UNSET,
        *,
        aliases: Any = (),
    ) -> None:
        aliases = list(aliases)
        for alias in aliases:
            if alias in ini_specs:
                raise ValueError(f"alias {alias!r} conflicts with existing configuration option")
            existing = ini_aliases.get(alias)
            if existing is not None and existing != name:
                raise ValueError(f"{alias!r} is already an alias of {existing!r}")
        ini_specs[name] = {"type": type, "default": default, "aliases": aliases, "help": help}
        for alias in aliases:
            ini_aliases[alias] = name


parser = Parser()


class _PytestHelpFormatter(argparse.HelpFormatter):
    """Matches upstream pytest's own --help formatting convention: a
    long option taking a value is joined to its metavar with '=' (e.g.
    '--allow-hosts=ALLOWED_HOSTS_CSV'), not argparse's default space-
    separated '--allow-hosts ALLOWED_HOSTS_CSV'."""

    def _format_action_invocation(self, action: argparse.Action) -> str:
        if not action.option_strings or action.nargs == 0:
            return super()._format_action_invocation(action)
        default_metavar = self._get_default_metavar_for_optional(action)
        (metavar,) = self._metavar_formatter(action, default_metavar)(1)
        return ", ".join(
            f"{opt}={metavar}" if opt.startswith("--") else opt for opt in action.option_strings
        )


# argparse kwargs Option.attrs() may carry that add_argument actually accepts;
# plugins pass through arbitrary extra keys (pytest-socket etc. don't, but
# nothing guarantees it), so this is an allowlist, not a denylist.
_ARGPARSE_KWARGS = frozenset(
    {
        "action",
        "default",
        "type",
        "choices",
        "help",
        "dest",
        "nargs",
        "metavar",
        "const",
        "required",
    }
)


def render_new_option_help(new_flags: list[str]) -> str:
    """--help text for flags added since a before/after `flag_dests` diff,
    grouped under each plugin's own `parser.getgroup(name)` heading (bare
    `parser.addoption()` calls render under 'custom options') — using
    argparse's own HelpFormatter so wrapping/alignment matches upstream
    pytest's --help output (which is itself argparse-rendered) exactly.
    """
    if not new_flags:
        return ""
    wanted = set(new_flags)
    seen_dests: set[str] = set()
    sections: list[str] = []
    for name, group in option_groups.items():
        matching = [opt for opt in group.options if any(n in wanted for n in opt.names())]
        if not matching:
            continue
        p = argparse.ArgumentParser(add_help=False, prog="-", formatter_class=_PytestHelpFormatter)
        title = name or "custom options"
        target = p.add_argument_group(title)
        for opt in matching:
            names = [n for n in opt.names() if n in wanted]
            if not names:
                continue
            dest = opt.attrs().get("dest") or names[0]
            if dest in seen_dests:
                continue
            seen_dests.add(dest)
            kwargs = {k: v for k, v in opt.attrs().items() if k in _ARGPARSE_KWARGS}
            try:
                target.add_argument(*names, **kwargs)
            except (argparse.ArgumentError, TypeError, ValueError):
                continue
        # format_help() always includes a leading "usage: ...\n\n" line this
        # throwaway single-group parser doesn't need; keep only the group's
        # own rendered section (everything from its title onward).
        text = p.format_help()
        marker = f"{title}:"
        idx = text.find(marker)
        if idx != -1:
            sections.append(text[idx:])
    return "\n".join(sections)


def _strtobool(value: str) -> bool:
    """pytest's bool ini conversion."""
    normalized = value.strip().lower()
    if normalized in ("y", "yes", "t", "true", "on", "1"):
        return True
    if normalized in ("n", "no", "f", "false", "off", "0"):
        return False
    raise ValueError(f"invalid truth value {value!r}")


#: Core inis with linelist semantics (pytest's builtin addini types):
#: getini returns a list of non-empty lines.
_LINELIST_INIS = {
    "markers",
    "filterwarnings",
    "norecursedirs",
    "testpaths",
    "python_files",
    "python_classes",
    "python_functions",
    "usefixtures",
}


#: The ini options pytest's core and builtin plugins register (name -> type).
#: Consulted only in strict mode (parseconfig-built configs) so getini can
#: tell a genuinely-unknown key from a core one the Rust engine owns.
_CORE_INI_TYPES: dict[str, str | None] = {
    "addopts": "args",
    "cache_dir": None,
    "collect_imported_tests": "bool",
    "consider_namespace_packages": "bool",
    "console_output_style": None,
    "disable_test_id_escaping_and_forfeit_all_rights_to_community_support": "bool",
    "doctest_encoding": None,
    "doctest_optionflags": "args",
    "empty_parameter_set_mark": None,
    "enable_assertion_pass_hook": "bool",
    "faulthandler_exit_on_timeout": "bool",
    "faulthandler_timeout": None,
    "filterwarnings": "linelist",
    "junit_duration_report": None,
    "junit_family": None,
    "junit_log_passing_tests": "bool",
    "junit_logging": None,
    "junit_suite_name": None,
    "log_auto_indent": None,
    "log_cli": "bool",
    "log_cli_date_format": None,
    "log_cli_format": None,
    "log_cli_level": None,
    "log_date_format": None,
    "log_file": None,
    "log_file_date_format": None,
    "log_file_format": None,
    "log_file_level": None,
    "log_file_mode": None,
    "log_format": None,
    "log_level": None,
    "markers": "linelist",
    "minversion": None,
    "norecursedirs": "args",
    "python_classes": "args",
    "python_files": "args",
    "python_functions": "args",
    "pythonpath": "paths",
    "pytester_example_dir": None,
    "required_plugins": "args",
    "strict": "bool",
    "strict_config": "bool",
    "strict_markers": "bool",
    "strict_parametrization_ids": "bool",
    "strict_xfail": "bool",
    "testpaths": "args",
    "tmp_path_retention_count": "string",
    "tmp_path_retention_policy": "string",
    "truncation_limit_chars": None,
    "truncation_limit_lines": None,
    "usefixtures": "args",
    "verbosity_assertions": "string",
    "verbosity_subtests": "string",
    "verbosity_test_cases": "string",
    "xfail_strict": "bool",
}


#: Upstream's own builtin ini options, in the exact order/wording its own
#: `parser.addini()` calls register them (so --help's ini listing matches
#: byte-for-byte) — (name, display type label, help text). Deliberately
#: excludes `pytester_example_dir` (`_pytest/pytester.py`): that ini is only
#: registered when the `pytester` builtin plugin is loaded, which upstream
#: does not do by default (it's in `builtin_plugins` but not
#: `default_plugins`) — a nested `pytester.runpytest()` session, like the one
#: `--help` conformance tests run under, never has it either.
_CORE_INI_HELP: list[tuple[str, str, str]] = [
    ("markers", "linelist", "Register new markers for test functions"),
    ("empty_parameter_set_mark", "string", "Default marker for empty parametersets"),
    (
        "strict_config",
        "bool",
        "Any warnings encountered while parsing the `pytest` section of the "
        "configuration file raise errors",
    ),
    (
        "strict_markers",
        "bool",
        "Markers not registered in the `markers` section of the configuration file raise errors",
    ),
    (
        "strict",
        "bool",
        "Enables all strictness options, currently: strict_config, "
        "strict_markers, strict_xfail, strict_parametrization_ids",
    ),
    (
        "filterwarnings",
        "linelist",
        "Each line specifies a pattern for warnings.filterwarnings. Processed "
        "after -W/--pythonwarnings.",
    ),
    ("norecursedirs", "args", "Directory patterns to avoid for recursion"),
    (
        "testpaths",
        "args",
        "Directories to search for tests when no files or directories are "
        "given on the command line",
    ),
    (
        "collect_imported_tests",
        "bool",
        "Whether to collect tests in imported modules outside `testpaths`",
    ),
    (
        "consider_namespace_packages",
        "bool",
        "Consider namespace packages when resolving module names during import",
    ),
    ("usefixtures", "args", "List of default fixtures to be used with this project"),
    (
        "python_files",
        "args",
        "Glob-style file patterns for Python test module discovery",
    ),
    (
        "python_classes",
        "args",
        "Prefixes or glob names for Python test class discovery",
    ),
    (
        "python_functions",
        "args",
        "Prefixes or glob names for Python test function and method discovery",
    ),
    (
        "disable_test_id_escaping_and_forfeit_all_rights_to_community_support",
        "bool",
        "Disable string escape non-ASCII characters, might cause unwanted "
        "side effects(use at your own risk)",
    ),
    (
        "strict_parametrization_ids",
        "bool",
        "Emit an error if non-unique parameter set IDs are detected",
    ),
    (
        "console_output_style",
        "string",
        'Console output: "classic", or with additional progress information '
        '("progress" (percentage) | "count" | "progress-even-when-capture-no" '
        "(forces progress even when capture=no)",
    ),
    (
        "verbosity_test_cases",
        "string",
        "Specify a verbosity level for test case execution, overriding the "
        "main level. Higher levels will provide more detailed information "
        "about each test case executed.",
    ),
    (
        "strict_xfail",
        "bool",
        "Default for the strict parameter of xfail markers when not given "
        "explicitly (default: False) (alias: xfail_strict)",
    ),
    (
        "tmp_path_retention_count",
        "string",
        "How many sessions should we keep the `tmp_path` directories, "
        "according to `tmp_path_retention_policy`.",
    ),
    (
        "tmp_path_retention_policy",
        "string",
        "Controls which directories created by the `tmp_path` fixture are "
        "kept around, based on test outcome. (all/failed/none)",
    ),
    (
        "enable_assertion_pass_hook",
        "bool",
        "Enables the pytest_assertion_pass hook. Make sure to delete any "
        "previously generated pyc cache files.",
    ),
    (
        "truncation_limit_lines",
        "string",
        "Set threshold of LINES after which truncation will take effect",
    ),
    (
        "truncation_limit_chars",
        "string",
        "Set threshold of CHARS after which truncation will take effect",
    ),
    (
        "verbosity_assertions",
        "string",
        "Specify a verbosity level for assertions, overriding the main level. "
        "Higher levels will provide more detailed explanation when an "
        "assertion fails.",
    ),
    ("junit_suite_name", "string", "Test suite name for JUnit report"),
    (
        "junit_logging",
        "string",
        "Write captured log messages to JUnit report: one of "
        "no|log|system-out|system-err|out-err|all",
    ),
    (
        "junit_log_passing_tests",
        "bool",
        "Capture log information for passing tests to JUnit report:",
    ),
    ("junit_duration_report", "string", "Duration time to report: one of total|call"),
    ("junit_family", "string", "Emit XML for schema: one of legacy|xunit1|xunit2"),
    ("doctest_optionflags", "args", "Option flags for doctests"),
    ("doctest_encoding", "string", "Encoding used for doctest files"),
    ("cache_dir", "string", "Cache directory path"),
    ("log_level", "string", "Default value for --log-level"),
    ("log_format", "string", "Default value for --log-format"),
    ("log_date_format", "string", "Default value for --log-date-format"),
    (
        "log_cli",
        "bool",
        'Enable log display during test run (also known as "live logging")',
    ),
    ("log_cli_level", "string", "Default value for --log-cli-level"),
    ("log_cli_format", "string", "Default value for --log-cli-format"),
    ("log_cli_date_format", "string", "Default value for --log-cli-date-format"),
    ("log_file", "string", "Default value for --log-file"),
    ("log_file_mode", "string", "Default value for --log-file-mode"),
    ("log_file_level", "string", "Default value for --log-file-level"),
    ("log_file_format", "string", "Default value for --log-file-format"),
    ("log_file_date_format", "string", "Default value for --log-file-date-format"),
    ("log_auto_indent", "string", "Default value for --log-auto-indent"),
    (
        "faulthandler_timeout",
        "string",
        "Dump the traceback of all threads if a test takes more than TIMEOUT seconds to finish",
    ),
    (
        "faulthandler_exit_on_timeout",
        "bool",
        "Exit the test process if a test takes more than faulthandler_timeout seconds to finish",
    ),
    (
        "verbosity_subtests",
        "string",
        "Specify verbosity level for subtests. Higher levels will generate "
        "output for passed subtests. Failed subtests are always reported.",
    ),
    ("addopts", "args", "Extra command line options"),
    ("minversion", "string", "Minimally required pytest version"),
    ("pythonpath", "paths", "Add paths to sys.path"),
    ("required_plugins", "args", "Plugins that must be present for pytest to run"),
]

#: `showhelp`'s fixed `Environment variables:` block (helpconfig.py), name -> help.
_ENV_VAR_HELP: list[tuple[str, str]] = [
    (
        "CI",
        "When set to a non-empty value, pytest knows it is running in a CI "
        "process and does not truncate summary info",
    ),
    ("BUILD_NUMBER", "Equivalent to CI"),
    ("PYTEST_ADDOPTS", "Extra command line options"),
    ("PYTEST_PLUGINS", "Comma-separated plugins to load during startup"),
    ("PYTEST_DISABLE_PLUGIN_AUTOLOAD", "Set to disable plugin auto-loading"),
    ("PYTEST_DEBUG", "Set to enable debug tracing of pytest's internals"),
    ("PYTEST_DEBUG_TEMPROOT", "Override the system temporary directory"),
    ("PYTEST_THEME", "The Pygments style to use for code output"),
    ("PYTEST_THEME_MODE", "Set the PYTEST_THEME to be either 'dark' or 'light'"),
]


def render_ini_help(columns: int) -> str:
    """The `[pytest] configuration options...` + `Environment variables:`
    section of `--help`, matching upstream's `_pytest.helpconfig.showhelp`
    algorithm exactly (including its open-line-buffer semantics: a spec with
    no help text, like an empty-string `addini` help, leaves its line open
    for the *next* write rather than emitting a blank line — significant for
    byte-parity when it's immediately followed by `Environment variables:`).

    Entries are `_CORE_INI_HELP` (upstream's own builtin inis, fixed order)
    followed by `ini_specs` in registration order (conftest/plugin `addini`
    calls) — deliberately never merged into `ini_specs` itself, since that
    dict's presence/absence drives `getini`'s registered-vs-unregistered
    return value (see `getini`'s docstring); this is a render-only view.

    Raises TypeError, matching upstream, if a registered ini's help is None
    (an `addini(name, None, ...)` call) — deferred to render time exactly
    like upstream's showhelp, not validated at addini() call time.
    """
    lines: list[str] = []
    current = ""

    def write(s: str) -> None:
        nonlocal current
        current += s

    def line(s: str = "") -> None:
        nonlocal current
        lines.append(current + s)
        current = ""

    line()
    line(
        "[pytest] configuration options in the first "
        "pytest.toml|pytest.ini|tox.ini|setup.cfg|pyproject.toml file found:"
    )
    line()

    indent_len = 24
    indent = " " * indent_len
    entries = list(_CORE_INI_HELP)
    entries.extend(
        (name, spec["type"] or "string", spec["help"]) for name, spec in ini_specs.items()
    )
    for name, type_, help_ in entries:
        if help_ is None:
            raise TypeError(f"help argument cannot be None for {name}")
        spec = f"{name} ({type_}):"
        write(f"  {spec}")
        spec_len = len(spec)
        if spec_len > (indent_len - 3):
            line()
            for wrapped_line in textwrap.wrap(
                help_,
                columns,
                initial_indent=indent,
                subsequent_indent=indent,
                break_on_hyphens=False,
            ):
                line(wrapped_line)
        else:
            write(" " * (indent_len - spec_len - 2))
            wrapped = textwrap.wrap(help_, columns - indent_len, break_on_hyphens=False)
            if wrapped:
                line(wrapped[0])
                for wrapped_line in wrapped[1:]:
                    line(indent + wrapped_line)

    line()
    line("Environment variables:")
    for name, help_ in _ENV_VAR_HELP:
        line(f"  {name:<24} {help_}")
    line()
    line()

    if current:
        lines.append(current)
    return "\n".join(lines) + "\n"


def _empty_for_type(type_: str | None) -> Any:
    """The default getini value for a registered ini with no value and no
    explicit default (pytest's per-type empty)."""
    if type_ == "bool":
        return False
    if type_ in ("args", "linelist", "paths", "pathlist"):
        return []
    # string / int / float / None
    return ""


def _split_str(value: str, shlex_split: bool) -> list:
    """Split a string ini value, detecting NUL-byte TOML array encoding.

    When the Rust engine stores a TOML array it joins elements with NUL bytes
    (``\\x00``) so multi-word items survive type coercion unchanged. Any value
    without NUL bytes came from a traditional ini file and is parsed with
    shlex (if shlex_split) or splitlines (for linelist types).

    A value can carry BOTH separators: a TOML-array ini (NUL-joined) that a
    plugin later appended to via ``config.addinivalue_line`` (newline-joined,
    e.g. pytest-django registering its ``django_db`` marker). Split on both so
    every entry comes out separate rather than the last array element being
    glued to the first appended line."""
    if "\x00" in value:
        parts = [p for chunk in value.split("\x00") for p in chunk.split("\n")]
        return [p for p in parts if p] if shlex_split else [p.strip() for p in parts if p.strip()]
    if shlex_split:
        return shlex.split(value)
    return [line.strip() for line in value.splitlines() if line.strip()]


#: TOML-native-value type names for each toml_type tag, keyed by what a
#: mismatching value's Python type name would read as in a TypeError message.
_TOML_TYPE_NAMES = {
    "string": "str",
    "int": "int",
    "float": "float",
    "bool": "bool",
    "array": "list",
}


def _validate_toml_type(type_: str | None, toml_type: str, value: Any, name: str) -> None:
    """Native pytest.toml/[tool.pytest] values keep their TOML type (str,
    int, float, bool, list) with no coercion. Validate strictly against the
    registered addini type, matching upstream's Config._getini_toml — a
    type mismatch is a TypeError, not silently coerced.

    ``toml_type`` for an array is ``"array:<item_type_0>\\x00<item_type_1>..."``
    (see render_toml_entries in ini.rs), giving each item's native TOML type
    without needing to re-inspect the already-stringified ``value``."""
    base_type = toml_type.split(":", 1)[0]
    got = _TOML_TYPE_NAMES.get(base_type, base_type)
    if type_ in ("paths", "args", "linelist"):
        if base_type != "array":
            raise TypeError(
                f"config option {name!r} expects a list for type {type_!r}, got {got}: {value!r}"
            )
        item_types = toml_type[len("array:") :].split("\x00") if ":" in toml_type else []
        items = value.split("\x00") if isinstance(value, str) else value
        for i, item_type in enumerate(item_types):
            if item_type != "string":
                item_type_name = _TOML_TYPE_NAMES.get(item_type, item_type)
                item_repr = items[i] if i < len(items) else ""
                raise TypeError(
                    f"config option {name!r} expects a list of strings, "
                    f"but item at index {i} is {item_type_name}: {item_repr!r}"
                )
    elif type_ == "bool":
        if toml_type != "bool":
            raise TypeError(f"config option {name!r} expects a bool, got {got}: {value!r}")
    elif type_ == "int":
        if toml_type != "int":
            raise TypeError(f"config option {name!r} expects an int, got {got}: {value!r}")
    elif type_ == "float":
        if toml_type not in ("float", "int"):
            raise TypeError(f"config option {name!r} expects a float, got {got}: {value!r}")
    elif type_ in ("string", None):
        if toml_type != "string":
            raise TypeError(f"config option {name!r} expects a string, got {got}: {value!r}")


def _coerce_ini(
    type_: str | None,
    value: Any,
    rootpath: str | None,
    name: str = "",
    toml_type: str | None = None,
) -> Any:
    """Coerce a raw ini value to its registered type (pytest INI-mode
    coercion). Values are strings from .ini files; toml linelists may already
    be lists. ``toml_type`` (set only for values sourced from a native
    pytest.toml/[tool.pytest] table) triggers strict type validation instead
    of coercion, matching upstream's TOML-mode getini."""
    if toml_type is not None:
        _validate_toml_type(type_, toml_type, value, name)
    if type_ == "paths":
        base = Path(rootpath) if rootpath else Path.cwd()
        parts = _split_str(value, True) if isinstance(value, str) else list(value)
        return [base / p for p in parts]
    if type_ == "pathlist":
        from pytest._tmp_path import LocalPath

        base = Path(rootpath) if rootpath else Path.cwd()
        parts = _split_str(value, True) if isinstance(value, str) else list(value)
        return [LocalPath(str(base / p)) for p in parts]
    if type_ == "args":
        return _split_str(value, True) if isinstance(value, str) else list(value)
    if type_ == "linelist":
        if isinstance(value, list):
            return value
        return _split_str(value, False)
    if type_ == "bool":
        return _strtobool(value.strip()) if isinstance(value, str) else bool(value)
    if type_ == "int":
        if not isinstance(value, str):
            raise TypeError(
                f"Expected an int string for option {name} of type integer, but got: {value!r}"
            )
        try:
            return int(value)
        except ValueError:
            raise TypeError(
                f"Expected an int string for option {name} of type integer, but got: {value!r}"
            ) from None
    if type_ == "float":
        if not isinstance(value, str):
            raise TypeError(
                f"Expected a float string for option {name} of type float, but got: {value!r}"
            )
        try:
            return float(value)
        except ValueError:
            raise TypeError(
                f"Expected a float string for option {name} of type float, but got: {value!r}"
            ) from None
    # string / None
    return value


def getini(
    name: str,
    inicfg: dict[str, str],
    rootpath: str | None,
    strict: bool = False,
    overrides: dict[str, str] | None = None,
    toml_types: dict[str, str] | None = None,
) -> Any:
    """config.getini(name): the typed, alias-resolved ini value. Registered
    options (parser.addini) supply type conversion and defaults.

    In strict mode (parseconfig-built configs) an unregistered, non-core key
    raises ValueError, matching upstream. The session config stays lenient —
    the Rust engine owns the core inis and never registers them here, so
    raising would regress its own getini calls.

    ``overrides`` (the raw -o/--override-ini values) is checked before
    ``inicfg`` with full alias resolution so ``-o old_name=val`` wins over
    ``new_name = from_file`` when old_name is registered as an alias.

    ``toml_types`` maps key -> original TOML type tag, populated only for
    values sourced from a native pytest.toml/[tool.pytest] table (see
    render_toml_entries in ini.rs); it triggers strict type validation
    instead of ini-style string coercion (matches upstream's TOML mode)."""
    canonical = ini_aliases.get(name, name)
    spec = ini_specs.get(canonical)
    if spec is None:
        if strict:
            if canonical not in _CORE_INI_TYPES:
                raise ValueError(f"unknown configuration value: {name!r}")
            spec = {"type": _CORE_INI_TYPES[canonical], "default": _UNSET, "aliases": ()}
        else:
            # Unregistered: lenient fallback.
            raw = inicfg.get(name)
            if raw is None:
                return [] if name in _LINELIST_INIS else None
            if name in _LINELIST_INIS:
                return _split_str(raw, False)
            return raw
    type_ = spec["type"]
    # Override precedence: -o canonical first, then any alias. -o values are
    # always plain CLI strings, never TOML-sourced, so no toml_type here.
    if overrides is not None:
        override_val = overrides.get(canonical)
        if override_val is None:
            for alias in spec.get("aliases", ()):
                if alias in overrides:
                    override_val = overrides[alias]
                    break
        if override_val is not None:
            return _coerce_ini(type_, override_val, rootpath, canonical)
    # Value precedence: canonical name first, then any alias.
    value = inicfg.get(canonical)
    matched_key = canonical if value is not None else None
    if value is None:
        for alias in spec.get("aliases", ()):
            if inicfg.get(alias) is not None:
                value = inicfg[alias]
                matched_key = alias
                break
    if value is None:
        default = spec["default"]
        return _empty_for_type(type_) if default is _UNSET else default
    toml_type = toml_types.get(matched_key) if toml_types and matched_key else None
    return _coerce_ini(type_, value, rootpath, canonical, toml_type)


#: Inis the bundled native plugins read (registered in Rust via the plugin's
#: OptionParser, not the Python parser, so they don't appear in ini_specs).
#: Treated as known so unknown-config-option validation doesn't flag them.
_PLUGIN_INIS = {
    "anyio_backend",
    "anyio_mode",
    "asyncio_debug",
    "asyncio_default_fixture_loop_scope",
    "asyncio_default_test_loop_scope",
    "asyncio_mode",
    "mock_traceback_monkeypatch",
    "mock_use_standalone_module",
}


def unknown_ini_keys(inicfg_keys: Any) -> list[str]:
    """The config-file keys that are neither a registered (plugin/conftest
    addini) option nor a core/builtin one — pytest's unknown-config-option
    set (sorted)."""
    known = set(_CORE_INI_TYPES) | set(_PLUGIN_INIS) | set(ini_specs) | set(ini_aliases)
    return sorted(key for key in inicfg_keys if key not in known)


# Core pytest options that pytest-rs does not implement but plugins read
# defensively off config.option (e.g. pytest-rerunfailures checks
# config.option.usepdb to refuse reruns under the debugger).
CORE_OPTION_DEFAULTS: dict[str, Any] = {
    # assertion rewriting is pytest-rs's default; pytest-snapshot's test
    # helper reads config.option.assertmode to pick runpytest vs subprocess.
    "assertmode": "rewrite",
    # core flags plugins read off a parse_known_args namespace (pytest-django).
    # "help" has its own option_specs entry (registered above) instead of a
    # CORE_OPTION_DEFAULTS one, since it needs apply_cli_args to actually set
    # it from argv, not just supply a static default.
    "version": 0,
    # --fixtures / --funcargs dest; the engine sets it true when either is given.
    "showfixtures": False,
}


def option_lookup(dest: str) -> tuple[bool, Any]:
    """(registered, default) for one option dest — registered defaults win
    over the getoption(default=) argument, like pytest's parsed namespace."""
    spec = option_specs.get(dest)
    if spec is None:
        if dest in CORE_OPTION_DEFAULTS:
            return (True, CORE_OPTION_DEFAULTS[dest])
        return (False, None)
    return (True, spec["default"])


class OptionNamespace:
    """config.option: explicit attributes win; unset names fall back to the
    plugin-registered option defaults (pytest's argparse namespace carries
    every registered option's default, e.g. sugar's `config.option.tb_summary`)."""

    def __getattr__(self, name: str) -> Any:
        # Only reached when the attribute is not set on the instance.
        registered, default = option_lookup(name)
        if registered:
            return default
        raise AttributeError(name)


def apply_cli_args(namespace: Any, tokens: list[str]) -> tuple[list[str], list[str]]:
    """Apply deferred CLI tokens (`--flag=value`, or `--flag` optionally
    followed by its separate value token) against the registered option
    specs, setting converted values on config.option.  Returns
    (unknown_flags, leftover_positionals): unknown_flags are ``--flag``
    tokens no plugin registered (the engine usage-errors on them);
    leftover_positionals are non-flag tokens that were eagerly consumed
    during partitioning but turned out to be positional test-path args
    (e.g. ``--fail test_a.py`` where ``--fail`` is store_true)."""
    unknown = []
    positionals = []
    index = 0
    while index < len(tokens):
        token = tokens[index]
        index += 1
        if not token.startswith("--"):
            positionals.append(token)
            continue
        name, eq, value = token.partition("=")
        dest = flag_dests.get(name)
        if dest is None:
            unknown.append(token)
            continue
        spec = option_specs[dest]
        # Use the per-flag action so two flags sharing a dest but with
        # different actions work correctly (e.g. --nomigrations=store_true and
        # --migrations=store_false both with dest="nomigrations").
        action = flag_actions.get(name) or spec["action"]
        if action in ("store_true", "store_false"):
            setattr(namespace, dest, action == "store_true")
            continue
        convert = spec["type"]
        nargs = spec.get("nargs")
        # nargs=N collects a list; the single-value branch a scalar.
        converted: object
        # nargs=N consumes N value tokens (pytest-metadata's `--metadata k v`).
        if isinstance(nargs, int) and nargs > 1:
            collected = []
            if eq:
                collected.append(value)
            while len(collected) < nargs and index < len(tokens):
                collected.append(tokens[index])
                index += 1
            if len(collected) < nargs:
                from pytest import UsageError

                plural = "s" if nargs != 1 else ""
                raise UsageError(
                    f"pytest: error: argument {name}: expected {nargs} argument{plural}"
                )
            try:
                converted = [convert(v) if callable(convert) else v for v in collected]
            except (ValueError, argparse.ArgumentTypeError) as exc:
                from pytest import UsageError

                raise UsageError(f"pytest: error: argument {name}: {exc}") from None
        else:
            if not eq:
                if index < len(tokens) and not tokens[index].startswith("--"):
                    value = tokens[index]
                    index += 1
                else:
                    from pytest import UsageError

                    raise UsageError(f"pytest: error: argument {name}: expected one argument")
            try:
                converted = convert(value) if callable(convert) else value
            except (ValueError, argparse.ArgumentTypeError) as exc:
                from pytest import UsageError

                raise UsageError(f"pytest: error: argument {name}: {exc}") from None
        # Validate choices (argparse-compatible behaviour).
        choices = spec.get("choices")
        if choices is not None:
            check_vals = converted if isinstance(converted, list) else [converted]
            for val in check_vals:
                if val not in choices:
                    from pytest import UsageError

                    choices_str = ", ".join(repr(c) for c in choices)
                    raise UsageError(
                        f"pytest: error: argument {name}: invalid choice: {val!r} (choose from {choices_str})"
                    )
        # action="append" accumulates into a list (default []).
        if spec["action"] == "append":
            existing = getattr(namespace, dest, None)
            if not isinstance(existing, list):
                existing = list(spec["default"] or [])
            existing.append(converted)
            setattr(namespace, dest, existing)
        else:
            setattr(namespace, dest, converted)
    return unknown, positionals
