"""The importable subset of upstream's assertion-rewriting import hook.

pytest-rs rewrites asserts natively during collection; this hook exists
for suites that import extra modules through it directly — e.g. a
conftest fixture building modules with rewritten assertions via
``AssertionRewritingHook(config=...)`` as the explicit loader of
``importlib.util.spec_from_file_location``.
"""

from __future__ import annotations

import importlib.machinery
import importlib.util
from typing import Any

from _pytest._stub import __getattr__  # noqa: F401  (other internals stay stubbed)


class AssertionRewritingHook:
    """importlib meta-path finder/loader that assert-rewrites the modules
    it loads (the engine's rewriter does the work)."""

    def __init__(self, config: Any = None) -> None:
        self.config = config
        self._marked: set[str] = set()

    def mark_rewrite(self, *names: str) -> None:
        """Modules named here are rewritten even without a plugin marker
        (upstream API; the explicit-loader path rewrites unconditionally)."""
        self._marked.update(names)

    # -- meta path finder ------------------------------------------------
    def find_spec(self, name, path=None, target=None):
        spec = importlib.machinery.PathFinder.find_spec(name, path, target)
        if spec is None or spec.origin is None or not spec.origin.endswith(".py"):
            return None
        return importlib.util.spec_from_file_location(
            spec.name,
            spec.origin,
            loader=self,
            submodule_search_locations=spec.submodule_search_locations,
        )

    # -- loader ----------------------------------------------------------
    def create_module(self, spec):
        return None  # default module creation semantics

    def exec_module(self, module) -> None:
        from pytest import _rewrite

        spec = module.__spec__
        loader = _rewrite._RewriteLoader(spec.name, spec.origin)
        loader.exec_module(module)
