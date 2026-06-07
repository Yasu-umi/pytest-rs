"""Parser shim for plugin/conftest pytest_addoption hooks: records option
and ini specs so config.getoption()/getini() can fall back to the
plugin-declared defaults (e.g. pytest-timeout's `timeout_func_only`
bool ini defaulting to False). CLI parsing of plugin-defined flags is not
wired up yet — unknown flags still error at the Rust argument parser."""

from __future__ import annotations

from typing import Any

# name -> {"type": str | None, "default": Any}
ini_specs: dict[str, dict[str, Any]] = {}
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
        default: Any = None,
    ) -> None:
        if type == "bool" and default is None:
            default = False
        ini_specs[name] = {"type": type, "default": default}


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


def ini_lookup(name: str, raw: str | None) -> Any:
    """Resolve one ini value: the configured string converted per the
    registered spec's type, or the spec default when unset."""
    spec = ini_specs.get(name)
    if raw is None:
        if spec is not None:
            return spec["default"]
        return [] if name in _LINELIST_INIS else None
    if spec is not None and spec["type"] == "bool":
        return _strtobool(raw)
    if spec is None and name in _LINELIST_INIS:
        return [line.strip() for line in raw.splitlines() if line.strip()]
    return raw


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
