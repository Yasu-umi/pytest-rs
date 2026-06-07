"""xdist controller<->worker data exchange.

The controller creates one WorkerNode per worker and fires
pytest_configure_node(node) before the worker starts; node.workerinput
travels to the worker (config.workerinput). The worker fills
config.workeroutput, which travels back and surfaces as node.workeroutput
in pytest_testnodedown(node, error).
"""

from __future__ import annotations

from typing import Any

# The worker process's config.workeroutput (sent to the controller at
# shutdown).
workeroutput: dict = {}


class _Gateway:
    """Stand-in for the execnet gateway (only .id is commonly used)."""

    def __init__(self, gid: str) -> None:
        self.id = gid

    def __repr__(self) -> str:
        return f"<Gateway id={self.id!r}>"


class WorkerNode:
    """Controller-side stand-in for xdist's WorkerController (the `node`
    argument of pytest_configure_node / pytest_testnodedown)."""

    def __init__(self, gid: str, config: Any, workerinput: dict) -> None:
        self.gateway = _Gateway(gid)
        self.config = config
        self.workerinput = workerinput
        self.workeroutput: dict = {}

    def __repr__(self) -> str:
        return f"<WorkerNode {self.gateway.id}>"
