"""rootdir/inifile discovery: a port of pytest's _pytest/config/findpaths.py
(upstream unit-tests these functions directly)."""

import os
from dataclasses import KW_ONLY, dataclass
from pathlib import Path

import iniconfig

from pytest import UsageError
from pytest._outcomes import fail


def absolutepath(path):
    """Convert a path to an absolute path using os.path.abspath (does not
    resolve symlinks)."""
    return Path(os.path.abspath(str(path)))


def commonpath(path1, path2):
    """Return the common part shared with the other path, or None if there
    is no common part."""
    try:
        return Path(os.path.commonpath((str(path1), str(path2))))
    except ValueError:
        return None


def safe_exists(p):
    """Like Path.exists(), but account for input arguments that might be
    too long (#11394)."""
    try:
        return p.exists()
    except (ValueError, OSError):
        return False


@dataclass(frozen=True)
class ConfigValue:
    """Represents a configuration value with its origin and parsing mode."""

    value: object
    _: KW_ONLY
    origin: str  # "file" | "override"
    mode: str  # "ini" | "toml"


def _parse_ini_config(path):
    """Parse the given generic '.ini' file using legacy IniConfig parser,
    returning the parsed object. Raise UsageError if the file cannot be
    parsed."""
    try:
        return iniconfig.IniConfig(str(path))
    except iniconfig.ParseError as exc:
        raise UsageError(str(exc)) from exc


CFG_PYTEST_SECTION = "[pytest] section in {filename} files is no longer supported, change to [tool:pytest] instead."


def load_config_dict_from_file(filepath):
    """Load pytest configuration from the given file path, if supported.

    Return None if the file does not contain valid pytest configuration.
    """
    # Configuration from ini files are obtained from the [pytest] section,
    # if present.
    if filepath.suffix == ".ini":
        iniconfig_ = _parse_ini_config(filepath)

        if "pytest" in iniconfig_:
            return {
                k: ConfigValue(v, origin="file", mode="ini")
                for k, v in iniconfig_["pytest"].items()
            }
        else:
            # "pytest.ini" files are always the source of configuration,
            # even if empty.
            if filepath.name in {"pytest.ini", ".pytest.ini"}:
                return {}

    # '.cfg' files are considered if they contain a "[tool:pytest]" section.
    elif filepath.suffix == ".cfg":
        iniconfig_ = _parse_ini_config(filepath)

        if "tool:pytest" in iniconfig_.sections:
            return {
                k: ConfigValue(v, origin="file", mode="ini")
                for k, v in iniconfig_["tool:pytest"].items()
            }
        elif "pytest" in iniconfig_.sections:
            # Plain "[pytest]" sections in setup.cfg files are no longer
            # supported (#3086).
            fail(CFG_PYTEST_SECTION.format(filename="setup.cfg"), pytrace=False)

    # '.toml' files are considered if they contain a [tool.pytest] table
    # (toml mode) or [tool.pytest.ini_options] table (ini mode) for
    # pyproject.toml, or [pytest] table (toml mode) for pytest.toml.
    elif filepath.suffix == ".toml":
        import tomllib

        toml_text = filepath.read_text(encoding="utf-8")
        try:
            config = tomllib.loads(toml_text)
        except tomllib.TOMLDecodeError as exc:
            raise UsageError(f"{filepath}: {exc}") from exc

        # pytest.toml and .pytest.toml use [pytest] table directly.
        if filepath.name in ("pytest.toml", ".pytest.toml"):
            pytest_config = config.get("pytest", {})
            if pytest_config:
                # TOML mode - preserve native TOML types.
                return {
                    k: ConfigValue(v, origin="file", mode="toml")
                    for k, v in pytest_config.items()
                }
            # "pytest.toml" files are always the source of configuration,
            # even if empty.
            return {}

        # pyproject.toml uses [tool.pytest] or [tool.pytest.ini_options].
        else:
            tool_pytest = config.get("tool", {}).get("pytest", {})

            toml_config = {k: v for k, v in tool_pytest.items() if k != "ini_options"}
            ini_config = tool_pytest.get("ini_options", None)

            if toml_config and ini_config:
                raise UsageError(
                    f"{filepath}: Cannot use both [tool.pytest] (native TOML types) and "
                    "[tool.pytest.ini_options] (string-based INI format) simultaneously. "
                    "Please use [tool.pytest] with native TOML types (recommended) "
                    "or [tool.pytest.ini_options] for backwards compatibility."
                )

            if toml_config:
                # TOML mode - preserve native TOML types.
                return {
                    k: ConfigValue(v, origin="file", mode="toml")
                    for k, v in toml_config.items()
                }

            elif ini_config is not None:
                # INI mode - convert all scalar values to str for
                # compatibility with the INI system.
                def make_scalar(v):
                    return v if isinstance(v, list) else str(v)

                return {
                    k: ConfigValue(make_scalar(v), origin="file", mode="ini")
                    for k, v in ini_config.items()
                }

    return None


def locate_config(invocation_dir, args):
    """Search in the list of arguments for a valid ini-file for pytest,
    and return a tuple of (rootdir, inifile, cfg-dict, ignored-config-files),
    where ignored-config-files is a list of config basenames found that
    contain pytest configuration but were ignored."""
    config_names = [
        "pytest.toml",
        ".pytest.toml",
        "pytest.ini",
        ".pytest.ini",
        "pyproject.toml",
        "tox.ini",
        "setup.cfg",
    ]
    args = [x for x in args if not str(x).startswith("-")]
    if not args:
        args = [invocation_dir]
    found_pyproject_toml = None
    ignored_config_files = []

    for arg in args:
        argpath = absolutepath(arg)
        for base in (argpath, *argpath.parents):
            for config_name in config_names:
                p = base / config_name
                if p.is_file():
                    if p.name == "pyproject.toml" and found_pyproject_toml is None:
                        found_pyproject_toml = p
                    ini_config = load_config_dict_from_file(p)
                    if ini_config is not None:
                        index = config_names.index(config_name)
                        for remainder in config_names[index + 1 :]:
                            p2 = base / remainder
                            if (
                                p2.is_file()
                                and load_config_dict_from_file(p2) is not None
                            ):
                                ignored_config_files.append(remainder)
                        return base, p, ini_config, ignored_config_files
    if found_pyproject_toml is not None:
        return found_pyproject_toml.parent, found_pyproject_toml, {}, []
    return None, None, {}, []


def get_common_ancestor(invocation_dir, paths):
    common_ancestor = None
    for path in paths:
        if not path.exists():
            continue
        if common_ancestor is None:
            common_ancestor = path
        else:
            if common_ancestor in path.parents or path == common_ancestor:
                continue
            elif path in common_ancestor.parents:
                common_ancestor = path
            else:
                shared = commonpath(path, common_ancestor)
                if shared is not None:
                    common_ancestor = shared
    if common_ancestor is None:
        common_ancestor = invocation_dir
    elif common_ancestor.is_file():
        common_ancestor = common_ancestor.parent
    return common_ancestor


def get_dirs_from_args(args):
    def is_option(x):
        return x.startswith("-")

    def get_file_part_from_node_id(x):
        return x.split("::")[0]

    def get_dir_from_path(path):
        if path.is_dir():
            return path
        return path.parent

    # These look like paths but may not exist
    possible_paths = (
        absolutepath(get_file_part_from_node_id(arg))
        for arg in args
        if not is_option(arg)
    )

    return [get_dir_from_path(path) for path in possible_paths if safe_exists(path)]


def parse_override_ini(override_ini):
    """Parse the -o/--override-ini command line arguments and return the
    overrides.

    :raises UsageError: If one of the values is malformed.
    """
    overrides = {}
    # override_ini is a list of "ini=value" options; the last item wins.
    for ini_config in override_ini or ():
        try:
            key, user_ini_value = ini_config.split("=", 1)
        except ValueError as e:
            raise UsageError(
                f"-o/--override-ini expects option=value style (got: {ini_config!r})."
            ) from e
        else:
            overrides[key] = ConfigValue(user_ini_value, origin="override", mode="ini")
    return overrides


def determine_setup(*, inifile, override_ini, args, rootdir_cmd_arg, invocation_dir):
    """Determine the rootdir, inifile and ini configuration values from the
    command line arguments.

    :raises UsageError:
    """
    rootdir = None
    dirs = get_dirs_from_args(args)
    ignored_config_files = []

    if inifile:
        inipath_ = absolutepath(inifile)
        inipath = inipath_
        inicfg = load_config_dict_from_file(inipath_) or {}
        if rootdir_cmd_arg is None:
            rootdir = inipath_.parent
    else:
        ancestor = get_common_ancestor(invocation_dir, dirs)
        rootdir, inipath, inicfg, ignored_config_files = locate_config(
            invocation_dir, [ancestor]
        )
        if rootdir is None and rootdir_cmd_arg is None:
            for possible_rootdir in (ancestor, *ancestor.parents):
                if (possible_rootdir / "setup.py").is_file():
                    rootdir = possible_rootdir
                    break
            else:
                if dirs != [ancestor]:
                    rootdir, inipath, inicfg, _ = locate_config(invocation_dir, dirs)
                if rootdir is None:
                    rootdir = get_common_ancestor(
                        invocation_dir, [invocation_dir, ancestor]
                    )
                    if is_fs_root(rootdir):
                        rootdir = ancestor
    if rootdir_cmd_arg:
        rootdir = absolutepath(os.path.expandvars(rootdir_cmd_arg))
        if not rootdir.is_dir():
            raise UsageError(
                f"Directory '{rootdir}' not found. Check your '--rootdir' option."
            )

    ini_overrides = parse_override_ini(override_ini)
    inicfg.update(ini_overrides)

    assert rootdir is not None
    return rootdir, inipath, inicfg, ignored_config_files


def is_fs_root(p):
    r"""Return True if the given path is pointing to the root of the file
    system ("/" on Unix and "C:\\" on Windows for example)."""
    return os.path.splitdrive(str(p))[1] == os.sep


from _pytest._stub import __getattr__  # noqa: E402, F401
