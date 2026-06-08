"""Parser shim for plugin/conftest pytest_addoption hooks: records option
and ini specs so config.getoption()/getini() can fall back to the
plugin-declared defaults (e.g. pytest-timeout's `timeout_func_only`
bool ini defaulting to False). CLI parsing of plugin-defined flags is not
wired up yet — unknown flags still error at the Rust argument parser."""

from __future__ import annotations

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


class OptionGroup:
    def __init__(self, parser: Parser) -> None:
        self.parser = parser

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
        }
        for opt in opts:
            if opt.startswith("--"):
                flag_dests[opt] = dest

    _addoption = addoption

    def addini(self, *args: Any, **kwargs: Any) -> None:
        self.parser.addini(*args, **kwargs)


class Parser:
    def getgroup(self, name: str, description: str = "", after: str | None = None) -> OptionGroup:
        return OptionGroup(self)

    def addoption(self, *opts: str, **attrs: Any) -> None:
        OptionGroup(self).addoption(*opts, **attrs)

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
        ini_specs[name] = {"type": type, "default": default, "aliases": aliases}
        for alias in aliases:
            ini_aliases[alias] = name


parser = Parser()


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


def _empty_for_type(type_: str | None) -> Any:
    """The default getini value for a registered ini with no value and no
    explicit default (pytest's per-type empty)."""
    if type_ == "bool":
        return False
    if type_ in ("args", "linelist", "paths", "pathlist"):
        return []
    # string / int / float / None
    return ""


def _coerce_ini(type_: str | None, value: Any, rootpath: str | None) -> Any:
    """Coerce a raw ini value to its registered type (pytest INI-mode
    coercion). Values are strings from .ini files; toml linelists may already
    be lists."""
    import shlex
    from pathlib import Path

    if type_ == "paths":
        base = Path(rootpath) if rootpath else Path.cwd()
        parts = shlex.split(value) if isinstance(value, str) else list(value)
        return [base / p for p in parts]
    if type_ == "pathlist":
        from pytest._tmp_path import LocalPath

        base = Path(rootpath) if rootpath else Path.cwd()
        parts = shlex.split(value) if isinstance(value, str) else list(value)
        return [LocalPath(str(base / p)) for p in parts]
    if type_ == "args":
        return shlex.split(value) if isinstance(value, str) else list(value)
    if type_ == "linelist":
        if isinstance(value, list):
            return value
        return [line.strip() for line in value.splitlines() if line.strip()]
    if type_ == "bool":
        return _strtobool(value.strip()) if isinstance(value, str) else bool(value)
    if type_ == "int":
        return int(value)
    if type_ == "float":
        return float(value)
    # string / None
    return value


def getini(name: str, inicfg: dict[str, str], rootpath: str | None, strict: bool = False) -> Any:
    """config.getini(name): the typed, alias-resolved ini value. Registered
    options (parser.addini) supply type conversion and defaults.

    In strict mode (parseconfig-built configs) an unregistered, non-core key
    raises ValueError, matching upstream. The session config stays lenient —
    the Rust engine owns the core inis and never registers them here, so
    raising would regress its own getini calls."""
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
                return [line.strip() for line in raw.splitlines() if line.strip()]
            return raw
    type_ = spec["type"]
    # Value precedence: canonical name first, then any alias.
    value = inicfg.get(canonical)
    if value is None:
        for alias in spec.get("aliases", ()):
            if inicfg.get(alias) is not None:
                value = inicfg[alias]
                break
    if value is None:
        default = spec["default"]
        return _empty_for_type(type_) if default is _UNSET else default
    return _coerce_ini(type_, value, rootpath)


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
    "usepdb": False,
    "usepdb_cls": None,
    # assertion rewriting is pytest-rs's default; pytest-snapshot's test
    # helper reads config.option.assertmode to pick runpytest vs subprocess.
    "assertmode": "rewrite",
    # core flags plugins read off a parse_known_args namespace (pytest-django).
    "version": 0,
    "help": False,
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


def apply_cli_args(namespace: Any, tokens: list[str]) -> list[str]:
    """Apply deferred CLI tokens (`--flag=value`, or `--flag` optionally
    followed by its separate value token) against the registered option
    specs, setting converted values on config.option. Returns the tokens no
    plugin registered (the engine usage-errors on them, like pytest's
    "unrecognized arguments")."""
    unknown = []
    index = 0
    while index < len(tokens):
        token = tokens[index]
        index += 1
        if not token.startswith("--"):
            # A value token its flag didn't consume (store_true followed by
            # a positional, or an unknown flag's value).
            unknown.append(token)
            continue
        name, eq, value = token.partition("=")
        dest = flag_dests.get(name)
        if dest is None:
            unknown.append(token)
            continue
        spec = option_specs[dest]
        if spec["action"] in ("store_true", "store_false"):
            setattr(namespace, dest, spec["action"] == "store_true")
            continue
        convert = spec["type"]
        cast = (lambda v: convert(v)) if callable(convert) else (lambda v: v)
        nargs = spec.get("nargs")
        # nargs=N consumes N value tokens (pytest-metadata's `--metadata k v`).
        if isinstance(nargs, int) and nargs > 1:
            collected = []
            if eq:
                collected.append(value)
            while len(collected) < nargs and index < len(tokens):
                collected.append(tokens[index])
                index += 1
            if len(collected) < nargs:
                unknown.append(token)
                continue
            converted = [cast(v) for v in collected]
        else:
            if not eq:
                if index < len(tokens) and not tokens[index].startswith("--"):
                    value = tokens[index]
                    index += 1
                else:
                    unknown.append(token)
                    continue
            converted = cast(value)
        # action="append" accumulates into a list (default []).
        if spec["action"] == "append":
            existing = getattr(namespace, dest, None)
            if not isinstance(existing, list):
                existing = list(spec["default"] or [])
            existing.append(converted)
            setattr(namespace, dest, existing)
        else:
            setattr(namespace, dest, converted)
    return unknown
