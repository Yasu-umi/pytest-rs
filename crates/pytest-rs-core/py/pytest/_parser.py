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


def getini(name: str, inicfg: dict[str, str], rootpath: str | None) -> Any:
    """config.getini(name): the typed, alias-resolved ini value. Registered
    options (parser.addini) supply type conversion and defaults; unregistered
    names fall back to the lenient core behavior (a linelist for known core
    inis, else the raw string or None)."""
    canonical = ini_aliases.get(name, name)
    spec = ini_specs.get(canonical)
    if spec is None:
        # Unregistered: lenient fallback (strict ValueError is upstream's
        # behavior but would regress core inis the engine never registers).
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


def option_lookup(dest: str) -> tuple[bool, Any]:
    """(registered, default) for one option dest — registered defaults win
    over the getoption(default=) argument, like pytest's parsed namespace."""
    spec = option_specs.get(dest)
    if spec is None:
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
        if not eq:
            if index < len(tokens) and not tokens[index].startswith("--"):
                value = tokens[index]
                index += 1
            else:
                unknown.append(token)
                continue
        convert = spec["type"]
        setattr(namespace, dest, convert(value) if callable(convert) else value)
    return unknown
