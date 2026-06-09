"""Hook relay plugin injected into child subprocess runs.

When PYTEST_RS_HOOK_RELAY is set, this plugin records selected hook events
and writes them as JSON so the parent InlineRunResult can implement getcalls().
"""

import json
import os


class _HookRelayPlugin:
    def __init__(self, relay_path):
        self._relay_path = relay_path
        self._events = []

    def pytest_deselected(self, items):
        self._events.append({
            "hook": "pytest_deselected",
            "items": [{"name": i.name, "nodeid": i.nodeid} for i in items],
        })

    def pytest_collection_finish(self, session):
        self._events.append({
            "hook": "pytest_collection_finish",
            "session_items": [{"name": i.name, "nodeid": i.nodeid} for i in session.items],
        })
        # Write relay here: collection_finish fires after pytest_deselected
        # and is dispatched to instance hooks, so all deselected data is ready.
        with open(self._relay_path, "w", encoding="utf-8") as f:
            json.dump(self._events, f)


def pytest_configure(config):
    relay_path = os.environ.get("PYTEST_RS_HOOK_RELAY")
    if relay_path:
        config.pluginmanager.register(_HookRelayPlugin(relay_path))
