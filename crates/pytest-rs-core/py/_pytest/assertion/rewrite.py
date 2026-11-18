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
import marshal
import os
import types
from collections.abc import Callable
from pathlib import Path
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


# The rewritten-bytecode cache I/O pytest-rs's import hook
# (pytest._rewrite._RewriteLoader) runs. They live here, under upstream's
# module path and function names, because tools allowlist pytest's own
# rewriting I/O by exactly that frame identity: blockbuster (used by
# starlette's and anyio's suites) treats file reads in
# `_pytest/assertion/rewrite.py:_read_pyc` and writes in `:_write_pyc` /
# `:try_makedirs` as expected, but flags the same I/O anywhere else when a
# module is lazily imported inside an event loop. The signatures are
# pytest-rs's own (content-hash pycs), not upstream's.


def try_makedirs(cache_dir: Path | str) -> bool:
    """Create the __pycache__ directory; False when that is not possible."""
    try:
        os.makedirs(cache_dir, exist_ok=True)
    except OSError:
        return False
    return True


def _read_pyc(
    source: Path | str,
    pyc: Path | str,
    trace: Callable[[str], None] = lambda x: None,
    *,
    source_bytes: bytes | None = None,
) -> types.CodeType | None:
    """Return the cached code object iff the pyc's embedded source hash
    matches the source; otherwise None. Upstream's signature; pytest-rs's
    loader passes the source it already read as `source_bytes` so it is not
    read twice. The pytest-rs tag + CPython magic guard means a stale
    interpreter or non-rewritten pyc is never reused."""
    if source_bytes is None:
        try:
            with open(source, "rb") as fp:
                source_bytes = fp.read()
        except OSError:
            return None
    try:
        with open(pyc, "rb") as fp:
            data = fp.read()
    except OSError:
        return None
    if len(data) < 16 or data[:4] != importlib.util.MAGIC_NUMBER:
        return None
    if int.from_bytes(data[4:8], "little") != 0b11:  # hash-based, checked
        return None
    if data[8:16] != importlib.util.source_hash(source_bytes):
        return None
    try:
        # A memoryview slice rather than `data[16:]`, which would copy the
        # whole pyc once per imported module. Not `marshal.load(fp)`: that
        # reads the body in small chunks through the file object and
        # measured ~24% slower than parsing an in-memory buffer.
        code = marshal.loads(memoryview(data)[16:])
    except Exception:
        return None
    return code if isinstance(code, types.CodeType) else None


def _write_pyc(
    state: object,
    co: types.CodeType,
    source_stat: os.stat_result | None,
    pyc: Path | str,
    *,
    source_bytes: bytes | None = None,
) -> bool:
    """Write a content-hash pyc for `co`; True on success. Upstream's
    signature: pytest-rs keys pycs on the source's hash rather than
    `source_stat`, so without `source_bytes` there is nothing to key on and
    it skips caching."""
    if source_bytes is None:
        return False
    header = bytearray(importlib.util.MAGIC_NUMBER)
    header += (0b11).to_bytes(4, "little")  # hash-based + check_source flags
    header += importlib.util.source_hash(source_bytes)  # 8 bytes
    if not try_makedirs(os.path.dirname(pyc)):
        return False
    try:
        # Write to a process-unique temp then atomically rename, so parallel
        # workers sharing a __pycache__ never observe a half-written pyc.
        tmp = f"{pyc}.{os.getpid()}"
        with open(tmp, "wb") as fp:
            fp.write(header)
            fp.write(marshal.dumps(co))
        os.replace(tmp, pyc)
    except OSError:
        return False  # read-only dir etc.: skip caching, like upstream
    return True
