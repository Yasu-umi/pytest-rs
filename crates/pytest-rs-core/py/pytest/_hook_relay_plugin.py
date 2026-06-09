"""Hook relay plugin injected into child subprocess runs.

When PYTEST_RS_HOOK_RELAY is set, this plugin records selected hook events
and writes them as JSON so the parent InlineRunResult can implement getcalls().
"""

import atexit
import json
import os


class _HookRelayPlugin:
    def __init__(self, relay_path):
        self._relay_path = relay_path
        self._events = []
        self._written = False
        atexit.register(self._flush)

    def _flush(self):
        if not self._written:
            self._written = True
            try:
                with open(self._relay_path, "w", encoding="utf-8") as f:
                    json.dump(self._events, f)
            except Exception:
                pass

    def pytest_deselected(self, items):
        self._events.append({
            "hook": "pytest_deselected",
            "items": [{"name": i.name, "nodeid": i.nodeid} for i in items],
        })

    def pytest_collectreport(self, report):
        self._events.append({
            "hook": "pytest_collectreport",
            "nodeid": getattr(report, "nodeid", ""),
            "outcome": getattr(report, "outcome", ""),
            "longrepr": str(getattr(report, "longrepr", "") or ""),
        })

    def pytest_collection_finish(self, session):
        self._events.append({
            "hook": "pytest_collection_finish",
            "session_items": [{"name": i.name, "nodeid": i.nodeid} for i in session.items],
        })
        self._flush()


def pytest_configure(config):
    relay_path = os.environ.get("PYTEST_RS_HOOK_RELAY")
    if relay_path:
        config.pluginmanager.register(_HookRelayPlugin(relay_path))
