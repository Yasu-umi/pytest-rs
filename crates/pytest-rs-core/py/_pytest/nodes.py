from __future__ import annotations

from pathlib import Path

from pytest._node import Collector as Collector  # noqa: F401
from pytest._node import File as File  # noqa: F401
from pytest._node import Item as Item  # noqa: F401
from pytest._node import Node  # noqa: F401


def _check_initialpaths_for_relpath(
    initial_paths: frozenset, path: Path
) -> str | None:
    if path in initial_paths:
        return ""
    for parent in path.parents:
        if parent in initial_paths:
            return str(path.relative_to(parent))
    return None


from _pytest._stub import __getattr__  # noqa: E402, F401
