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
        try:
            with open(self._relay_path, "w", encoding="utf-8") as f:
                json.dump(self._events, f)
            self._written = True
        except Exception:
            pass

    def pytest_deselected(self, items):
        self._events.append(
            {
                "hook": "pytest_deselected",
                "items": [{"name": i.name, "nodeid": i.nodeid} for i in items],
            }
        )

    def pytest_collectstart(self, collector):
        self._events.append(
            {
                "hook": "pytest_collectstart",
                "collector_path": str(getattr(collector, "path", "") or ""),
                "collector_class": type(collector).__name__,
                "session_path": str(getattr(getattr(collector, "session", None), "path", "") or ""),
            }
        )

    def pytest_make_collect_report(self, collector):
        self._events.append(
            {
                "hook": "pytest_make_collect_report",
                "collector_path": str(getattr(collector, "path", "") or ""),
                "collector_class": type(collector).__name__,
            }
        )

    def pytest_pycollect_makeitem(self, collector, name, obj):
        self._events.append(
            {
                "hook": "pytest_pycollect_makeitem",
                "name": name,
                "collector_path": str(getattr(collector, "path", "") or ""),
            }
        )

    def pytest_collectreport(self, report):
        result_items = getattr(report, "result", []) or []
        result = [
            {
                "name": getattr(i, "name", ""),
                "nodeid": getattr(i, "nodeid", ""),
                "path": str(getattr(i, "path", "") or ""),
                "is_item": not hasattr(i, "collect"),
            }
            for i in result_items
            if hasattr(i, "name")
        ]
        self._events.append(
            {
                "hook": "pytest_collectreport",
                "nodeid": getattr(report, "nodeid", ""),
                "outcome": getattr(report, "outcome", ""),
                "longrepr": str(getattr(report, "longrepr", "") or ""),
                "result": result,
            }
        )

    def pytest_itemcollected(self, item):
        self._events.append(
            {
                "hook": "pytest_itemcollected",
                "nodeid": getattr(item, "nodeid", ""),
                "name": getattr(item, "name", ""),
                "path": str(getattr(item, "path", "") or ""),
            }
        )

    def pytest_collection_modifyitems(self, session, config, items):
        self._events.append(
            {
                "hook": "pytest_collection_modifyitems",
                "items": [
                    {
                        "nodeid": getattr(i, "nodeid", ""),
                        "name": getattr(i, "name", ""),
                        "path": str(getattr(i, "path", "") or ""),
                    }
                    for i in items
                ],
            }
        )

    def pytest_runtest_logstart(self, nodeid, location):
        self._events.append(
            {
                "hook": "pytest_runtest_logstart",
                "nodeid": nodeid,
                "location": list(location),
            }
        )

    def pytest_runtest_logfinish(self, nodeid, location):
        self._events.append(
            {
                "hook": "pytest_runtest_logfinish",
                "nodeid": nodeid,
                "location": list(location),
            }
        )

    def pytest_runtest_logreport(self, report):
        longrepr = getattr(report, "longrepr", None)
        longrepr_crash = None
        if hasattr(longrepr, "reprcrash") and longrepr.reprcrash is not None:
            crash = longrepr.reprcrash
            longrepr_crash = {
                "path": str(getattr(crash, "path", "")),
                "lineno": int(getattr(crash, "lineno", 0) or 0),
                "message": str(getattr(crash, "message", "")),
            }
        # Map pytest-rs's _LongRepr to ExceptionChainRepr so downstream code
        # (e.g. isinstance(rep.longrepr, ExceptionChainRepr)) works correctly.
        if longrepr_crash is not None:
            longrepr_type = "ExceptionChainRepr"
        else:
            longrepr_type = type(longrepr).__name__ if longrepr is not None else ""
        self._events.append(
            {
                "hook": "pytest_runtest_logreport",
                "nodeid": getattr(report, "nodeid", ""),
                "when": getattr(report, "when", ""),
                "outcome": getattr(report, "outcome", ""),
                "longrepr_type": longrepr_type,
                "longrepr_crash": longrepr_crash,
            }
        )

    def pytest_collection_finish(self, session):
        skipped_raw = getattr(session, "_rs_skipped_modules", None) or []
        self._events.append(
            {
                "hook": "pytest_collection_finish",
                "session_path": str(getattr(session, "path", "") or ""),
                "session_items": [
                    {
                        "name": i.name,
                        "nodeid": i.nodeid,
                        "path": str(getattr(i, "path", "") or ""),
                        "parent_class": (
                            type(i.parent).__name__
                            if getattr(i, "parent", None) is not None
                            else ""
                        ),
                    }
                    for i in session.items
                ],
                "skipped_modules": [
                    {"nodeid": m[0], "reason": m[1], "location": m[2]}
                    for m in skipped_raw
                ],
            }
        )
        # Don't flush here — run-phase events (logstart/logreport/logfinish)
        # come after collection. Final flush is in pytest_sessionfinish.

    def pytest_sessionfinish(self, session, exitstatus):
        self._flush()


def pytest_configure(config):
    relay_path = os.environ.get("PYTEST_RS_HOOK_RELAY")
    if relay_path:
        config.pluginmanager.register(_HookRelayPlugin(relay_path))
