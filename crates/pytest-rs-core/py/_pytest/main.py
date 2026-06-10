from __future__ import annotations

import argparse
import dataclasses
import importlib.util
import os
from collections.abc import Sequence
from pathlib import Path

from pytest import (
    ExitCode,  # noqa: F401
    UsageError,
)
from pytest._node import Session as Session  # noqa: E402, F401

from _pytest._stub import __getattr__  # noqa: E402, F401
from _pytest.pathlib import absolutepath, safe_exists


def _in_venv(path) -> bool:
    """Is this path the root of a virtual environment? (pyvenv.cfg or conda-meta check)"""
    try:
        p = str(path)
        return os.path.isfile(os.path.join(p, "pyvenv.cfg")) or os.path.isfile(
            os.path.join(p, "conda-meta", "history")
        )
    except OSError:
        return False


def validate_basetemp(path: str) -> str:
    """--basetemp validator: rejects empty, the cwd, or any cwd ancestor
    (pytest GH-7119). Faithful port."""
    msg = "basetemp must not be empty, the current working directory or any parent directory of it"

    if not path:
        raise argparse.ArgumentTypeError(msg)

    def is_ancestor(base: Path, query: Path) -> bool:
        if base == query:
            return True
        return query in base.parents

    if is_ancestor(Path.cwd(), Path(path).absolute()):
        raise argparse.ArgumentTypeError(msg)
    if is_ancestor(Path.cwd().resolve(), Path(path).resolve()):
        raise argparse.ArgumentTypeError(msg)

    return path


def search_pypath(module_name: str, *, consider_namespace_packages: bool = False) -> str | None:
    """Search sys.path for a dotted module name; return its filesystem path
    (pytest's search_pypath)."""
    try:
        spec = importlib.util.find_spec(module_name)
    except (AttributeError, ImportError, ValueError):
        return None
    if spec is None:
        return None
    if spec.submodule_search_locations is None or len(spec.submodule_search_locations) == 0:
        return spec.origin
    if consider_namespace_packages:
        return spec.submodule_search_locations[0]
    if spec.origin is None:
        return None
    return os.path.dirname(spec.origin)


@dataclasses.dataclass(frozen=True)
class CollectionArgument:
    """A resolved collection argument."""

    path: Path
    parts: Sequence[str]
    parametrization: str | None
    module_name: str | None
    original_index: int


def resolve_collection_argument(
    invocation_path: Path,
    arg: str,
    arg_index: int,
    *,
    as_pypath: bool = False,
    consider_namespace_packages: bool = False,
) -> CollectionArgument:
    """Parse a collection argument ("path::Class::test[id]" or, with
    as_pypath, "pkg.mod::...") into a resolved CollectionArgument. Faithful
    port of pytest's resolver."""
    base, squacket, rest = arg.partition("[")
    strpath, *parts = base.split("::")
    if squacket and not parts:
        raise UsageError(f"path cannot contain [] parametrization: {arg}")
    parametrization = f"{squacket}{rest}" if squacket else None
    module_name = None
    if as_pypath:
        pyarg_strpath = search_pypath(
            strpath, consider_namespace_packages=consider_namespace_packages
        )
        if pyarg_strpath is not None:
            module_name = strpath
            strpath = pyarg_strpath
    fspath = invocation_path / strpath
    fspath = absolutepath(fspath)
    if not safe_exists(fspath):
        msg = (
            "module or package not found: {arg} (missing __init__.py?)"
            if as_pypath
            else "file or directory not found: {arg}"
        )
        raise UsageError(msg.format(arg=arg))
    if parts and fspath.is_dir():
        msg = (
            "package argument cannot contain :: selection parts: {arg}"
            if as_pypath
            else "directory argument cannot contain :: selection parts: {arg}"
        )
        raise UsageError(msg.format(arg=arg))
    return CollectionArgument(
        path=fspath,
        parts=parts,
        parametrization=parametrization,
        module_name=module_name,
        original_index=arg_index,
    )
