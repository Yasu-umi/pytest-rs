"""pytester config-validation helpers (split out of _pytester.py)."""

from pytest._outcomes import fail


def _check_cfg_pytest_section(path, args) -> None:
    """Mimic upstream in-process behaviour: raise pytest.fail.Exception if any
    .cfg config file (auto-discovered or via -c/--config-file) contains a bare
    [pytest] section (which is no longer supported; users must use [tool:pytest])."""
    import configparser

    CFG_MSG = "[pytest] section in {filename} files is no longer supported, change to [tool:pytest] instead."

    def _has_pytest_section(cfg_path) -> bool:
        cp = configparser.ConfigParser()
        try:
            cp.read(str(cfg_path))
        except Exception:
            return False
        return "pytest" in cp and "tool:pytest" not in cp

    # Check explicit -c / --config-file argument first.
    explicit_cfg = None
    args_list = [str(a) for a in args]
    for i, arg in enumerate(args_list):
        if arg in ("-c", "--config-file") and i + 1 < len(args_list):
            explicit_cfg = path / args_list[i + 1]
            break
        if arg.startswith(("-c", "--config-file=")):
            val = arg.split("=", 1)[-1] if "=" in arg else arg[2:]
            if val:
                explicit_cfg = path / val
                break

    if explicit_cfg is not None:
        if explicit_cfg.suffix == ".cfg" and _has_pytest_section(explicit_cfg):
            fail(CFG_MSG.format(filename=explicit_cfg.name), pytrace=False)
        return  # explicit config file given — no auto-discovery

    # Auto-discovery: scan for .cfg files with [pytest] section.
    for cfg_file in path.glob("*.cfg"):
        if _has_pytest_section(cfg_file):
            fail(CFG_MSG.format(filename=cfg_file.name), pytrace=False)


def _validate_required_plugins(config) -> None:
    """Check required_plugins ini; raise UsageError if any are missing or version-mismatched."""
    import importlib.metadata

    try:
        required = config.getini("required_plugins")
    except Exception:
        return
    if not required:
        return

    try:
        from packaging.requirements import InvalidRequirement, Requirement
        from packaging.version import Version
    except ImportError:
        return

    import pytest

    dist_versions: dict = {}
    for dist in importlib.metadata.distributions():
        try:
            name = dist.metadata.get("name") or dist.metadata["name"]
            version = dist.version
            if name:
                dist_versions[name.lower()] = version
        except Exception:
            continue

    missing = []
    for req_str in required:
        try:
            req = Requirement(req_str)
        except InvalidRequirement:
            missing.append(req_str)
            continue
        name = req.name.lower()
        if name not in dist_versions:
            missing.append(req_str)
        elif req.specifier and not req.specifier.contains(
            Version(dist_versions[name]), prereleases=True
        ):
            missing.append(req_str)

    if missing:
        raise pytest.UsageError("Missing required plugins: {}".format(", ".join(missing)))
