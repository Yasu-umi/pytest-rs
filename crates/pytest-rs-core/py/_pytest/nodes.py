from __future__ import annotations

import abc
from pathlib import Path

from pytest._node import Collector as Collector  # noqa: F401
from pytest._node import File as File  # noqa: F401
from pytest._node import Item as Item  # noqa: F401
from pytest._node import Node as _InternalNode  # noqa: F401


class NodeMeta(abc.ABCMeta):
    """Metaclass that forbids direct construction of Node (use from_parent instead)."""

    def __call__(cls, *k, **kw):
        from _pytest.outcomes import fail
        msg = (
            f"Direct construction of {cls.__module__}.{cls.__qualname__} has been deprecated, "
            f"please use {cls.__module__}.{cls.__qualname__}.from_parent.\n"
            "See https://docs.pytest.org/en/stable/deprecations.html"
            "#node-construction-changed-to-node-from-parent for more details."
        )
        fail(msg, pytrace=False)

    def _create(cls, *k, **kw):
        return super().__call__(*k, **kw)


class Node(_InternalNode, metaclass=NodeMeta):
    """Public _pytest.nodes.Node: raises on direct construction (use from_parent)."""


def _check_initialpaths_for_relpath(initial_paths: frozenset, path: Path) -> str | None:
    if path in initial_paths:
        return ""
    for parent in path.parents:
        if parent in initial_paths:
            return str(path.relative_to(parent))
    return None


from _pytest._stub import __getattr__  # noqa: E402, F401
