"""pytester relay + inline-run result classes (split out of _pytester.py).

These reconstruct hook events / reports from the child run's relay file so
InlineRunResult.getcalls() works without an in-process pytest."""

import pathlib
import re
import sys

from pytest._node import Item as _Item


class _OutcomeReport:
    def __init__(self, nodeid, outcome=None, when="call", longrepr=None):
        self.nodeid = nodeid
        self.when = when
        self.outcome = outcome
        self.longrepr = longrepr

    @property
    def passed(self):
        return self.outcome == "passed"

    @property
    def skipped(self):
        return self.outcome == "skipped"

    @property
    def failed(self):
        return self.outcome == "failed"

    def __repr__(self):
        return f"<OutcomeReport {self.nodeid!r} {self.when} {self.outcome}>"


class _RelayItem:
    """Lightweight reconstruction of a pytest node from relay JSON data."""

    def __init__(self, name, nodeid, path=None):
        self.name = name
        self.nodeid = nodeid
        self.path = pathlib.Path(path) if path else None

    def __repr__(self):
        return f"<_RelayItem {self.nodeid!r}>"


class _RelayItemResult(_Item):
    """Relay item rebuilt from hook relay JSON; passes isinstance(x, pytest.Item)."""

    def __init__(self, name, nodeid, path=None):
        object.__setattr__(self, "name", name)
        object.__setattr__(self, "_nodeid", nodeid)
        _p = pathlib.Path(path).resolve() if path else None
        object.__setattr__(self, "path", _p)
        object.__setattr__(self, "own_markers", [])
        object.__setattr__(self, "parent", None)
        object.__setattr__(self, "config", None)

    @property  # type: ignore[misc]  # read-only view over the relayed nodeid
    def nodeid(self):
        return self._nodeid

    def __repr__(self):
        return f"<_RelayItemResult {self._nodeid!r}>"


class _RelayCollector:
    """Fake collector reconstructed from relay JSON (for assert_contains checks)."""

    def __init__(self, path, class_name, session_path):
        self.path = pathlib.Path(path) if path else None
        self._session_path = pathlib.Path(session_path) if session_path else None
        # Create a named subclass so __class__.__name__ == class_name
        self.__class__ = type(class_name, (_RelayCollector,), {})

    @property
    def session(self):
        return type("_S", (), {"path": self._session_path})()


class _RelaySession:
    """Lightweight reconstruction of session with .items from relay JSON data."""

    def __init__(self, items):
        self.items = items


class _RelayCollectReport:
    """Lightweight CollectReport reconstructed from relay JSON."""

    def __init__(self, nodeid, outcome, longrepr, result=None):
        self.nodeid = nodeid
        self.outcome = outcome
        self.longrepr = longrepr
        self.failed = outcome == "failed"
        self.passed = outcome == "passed"
        self.skipped = outcome == "skipped"
        self.result = result or []


class _RelayTestReport:
    """Lightweight TestReport reconstructed from relay JSON."""

    def __init__(self, nodeid, when, outcome, longrepr=None):
        self.nodeid = nodeid
        self.when = when
        self.outcome = outcome
        self.longrepr = longrepr

    @property
    def passed(self):
        return self.outcome == "passed"

    @property
    def failed(self):
        return self.outcome == "failed"

    @property
    def skipped(self):
        return self.outcome == "skipped"


class _RelayHookCall:
    """Reconstructed hook call record; named attributes come from relay JSON."""

    def __init__(self, hook_name, kwargs):
        self.__dict__.update(kwargs)
        self._name = hook_name

    def __repr__(self):
        d = {k: v for k, v in self.__dict__.items() if k != "_name"}
        return f"<_RelayHookCall {self._name!r}(**{d!r})>"

    @classmethod
    def _from_event(cls, event):
        hook = event["hook"]
        if hook == "pytest_deselected":
            items = [_RelayItem(i["name"], i["nodeid"]) for i in event.get("items", [])]
            return cls(hook, {"items": items})
        if hook == "pytest_collection_finish":
            items = [_RelayItem(i["name"], i["nodeid"]) for i in event.get("session_items", [])]
            return cls(hook, {"session": _RelaySession(items)})
        if hook == "pytest_collectreport":
            raw_result = event.get("result", []) or []
            result = [
                _RelayItemResult(r["name"], r["nodeid"], r.get("path"))
                if r.get("is_item", True)
                else _RelayItem(r["name"], r["nodeid"], r.get("path"))
                for r in raw_result
            ]
            report = _RelayCollectReport(
                event.get("nodeid", ""),
                event.get("outcome", ""),
                event.get("longrepr", ""),
                result,
            )
            return cls(hook, {"report": report})
        if hook in ("pytest_collectstart", "pytest_make_collect_report"):
            collector = _RelayCollector(
                event.get("collector_path", ""),
                event.get("collector_class", "collector"),
                event.get("session_path", ""),
            )
            return cls(hook, {"collector": collector})
        if hook == "pytest_pycollect_makeitem":
            return cls(
                hook,
                {
                    "name": event.get("name", ""),
                    "collector": _RelayCollector(event.get("collector_path", ""), "collector", ""),
                },
            )
        if hook in ("pytest_runtest_logstart", "pytest_runtest_logfinish"):
            location = event.get("location")
            if isinstance(location, list) and len(location) == 3:
                location = tuple(location)
            return cls(hook, {"nodeid": event.get("nodeid", ""), "location": location})
        if hook == "pytest_runtest_logreport":
            longrepr = None
            crash_data = event.get("longrepr_crash")
            if event.get("longrepr_type") == "ExceptionChainRepr" and crash_data:
                try:
                    from _pytest._code.code import (
                        ExceptionChainRepr,
                        ReprFileLocation,
                        ReprTraceback,
                    )

                    crash = ReprFileLocation(
                        path=crash_data.get("path", ""),
                        lineno=crash_data.get("lineno", 0),
                        message=crash_data.get("message", ""),
                    )
                    tb = ReprTraceback(reprentries=[], extraline=None, style="long")
                    longrepr = ExceptionChainRepr([(tb, crash, None)])
                except Exception:
                    longrepr = crash_data.get("message", "")
            report = _RelayTestReport(
                nodeid=event.get("nodeid", ""),
                when=event.get("when", ""),
                outcome=event.get("outcome", ""),
                longrepr=longrepr,
            )
            return cls(hook, {"report": report})
        return cls(hook, {k: v for k, v in event.items() if k != "hook"})


class InlineRunResult:
    """The subset of pytester's HookRecorder API used by upstream suites."""

    _WORDS = {
        "PASSED": "passed",
        "XPASS": "passed",
        "SKIPPED": "skipped",
        "XFAIL": "skipped",
        "FAILED": "failed",
        "ERROR": "failed",
    }

    def __init__(self, run_result, hook_events=None):
        self._result = run_result
        self.ret = run_result.ret
        self._hook_calls = self._build_calls(hook_events or [])

    @staticmethod
    def _build_calls(hook_events):
        """Convert relay events to _RelayHookCall list, synthesizing collection hooks."""
        calls = []
        for event in hook_events:
            if event.get("hook") != "pytest_collection_finish":
                calls.append(_RelayHookCall._from_event(event))
                continue
            # Synthesize collection hooks from pytest_collection_finish data
            session_path_str = event.get("session_path", "")
            session_path = pathlib.Path(session_path_str) if session_path_str else None
            raw_items = event.get("session_items", [])
            # Infer session_path from the first item's path + nodeid if not provided
            if session_path is None and raw_items:
                first = raw_items[0]
                item_path_str = first.get("path", "")
                nodeid = first.get("nodeid", "")
                if item_path_str and nodeid:
                    item_path = pathlib.Path(item_path_str)
                    file_part = nodeid.split("::")[0]
                    file_parts = pathlib.PurePosixPath(file_part).parts
                    # session_path = item_path parent raised by number of path components
                    if len(file_parts) >= 1:
                        session_path = item_path.parents[len(file_parts) - 1]
            # Group items by file (the path part of the nodeid, resolved via session_path)
            from collections import OrderedDict

            files_to_items = OrderedDict()
            for it in raw_items:
                nodeid = it.get("nodeid", "")
                item_path_str = it.get("path", "")
                if item_path_str:
                    file_path = pathlib.Path(item_path_str)
                elif session_path and nodeid:
                    file_part = nodeid.split("::")[0]
                    file_path = session_path / file_part
                else:
                    file_path = None
                key = str(file_path) if file_path else ""
                if key not in files_to_items:
                    files_to_items[key] = (file_path, [])
                files_to_items[key][1].append(it)

            if session_path is not None:
                # Session-level collectstart
                calls.append(
                    _RelayHookCall(
                        "pytest_collectstart",
                        {
                            "collector": _RelayCollector(
                                str(session_path), "Session", str(session_path)
                            )
                        },
                    )
                )
                # Session-level make_collect_report
                calls.append(
                    _RelayHookCall(
                        "pytest_make_collect_report",
                        {
                            "collector": _RelayCollector(
                                str(session_path), "Session", str(session_path)
                            )
                        },
                    )
                )
            _MODULE_CLASSES = {"", "NoneType", "Function", "Node", "Module"}
            for key, (file_path, file_items) in files_to_items.items():
                file_path_str = str(file_path) if file_path else ""
                session_path_str2 = str(session_path) if session_path else ""
                file_nodeid = (
                    file_items[0].get("nodeid", "").split("::")[0] if file_items else file_path_str
                )
                # Separate custom-collector items (parent_class != Module) from Module items
                from collections import OrderedDict as _OD

                custom_groups = _OD()
                module_items = []
                for it in file_items:
                    pc = it.get("parent_class", "")
                    if pc and pc not in _MODULE_CLASSES:
                        custom_groups.setdefault(pc, []).append(it)
                    else:
                        module_items.append(it)
                # Custom collectors: collectstart + make_collect_report + collectreport
                for parent_cls, custom_items in custom_groups.items():
                    calls.append(
                        _RelayHookCall(
                            "pytest_collectstart",
                            {
                                "collector": _RelayCollector(
                                    file_path_str, parent_cls, session_path_str2
                                )
                            },
                        )
                    )
                    calls.append(
                        _RelayHookCall(
                            "pytest_make_collect_report",
                            {
                                "collector": _RelayCollector(
                                    file_path_str, parent_cls, session_path_str2
                                )
                            },
                        )
                    )
                    custom_nodeid = custom_items[0].get("nodeid", "").split("::")[0]
                    custom_result = [
                        _RelayItemResult(
                            it.get("name", ""), it.get("nodeid", ""), it.get("path", "")
                        )
                        for it in custom_items
                    ]
                    calls.append(
                        _RelayHookCall(
                            "pytest_collectreport",
                            {
                                "report": _RelayCollectReport(
                                    custom_nodeid, "passed", "", custom_result
                                )
                            },
                        )
                    )
                # Module-level collectstart
                calls.append(
                    _RelayHookCall(
                        "pytest_collectstart",
                        {"collector": _RelayCollector(file_path_str, "Module", session_path_str2)},
                    )
                )
                # Module-level make_collect_report
                calls.append(
                    _RelayHookCall(
                        "pytest_make_collect_report",
                        {"collector": _RelayCollector(file_path_str, "Module", session_path_str2)},
                    )
                )
                # pycollect_makeitem only for Module-collected items
                for it in module_items:
                    item_name = it.get("name", "")
                    calls.append(
                        _RelayHookCall(
                            "pytest_pycollect_makeitem",
                            {
                                "name": item_name,
                                "collector": _RelayCollector(
                                    file_path_str, "Module", session_path_str2
                                ),
                            },
                        )
                    )
                # collectreport for module items
                module_nodeid = (
                    module_items[0].get("nodeid", "").split("::")[0]
                    if module_items
                    else file_nodeid
                )
                module_result = [
                    _RelayItemResult(it.get("name", ""), it.get("nodeid", ""), it.get("path", ""))
                    for it in module_items
                ]
                report = _RelayCollectReport(module_nodeid, "passed", "", module_result)
                calls.append(_RelayHookCall("pytest_collectreport", {"report": report}))

            # Append the original collection_finish
            calls.append(_RelayHookCall._from_event(event))

        return calls

    @property
    def calls(self):
        """All recorded hook calls (upstream HookRecorder.calls list)."""
        return self._hook_calls

    def getcalls(self, names):
        """Return recorded hook calls matching the given name(s) (space-sep string or list)."""
        if isinstance(names, str):
            names = names.split()
        return [c for c in self._hook_calls if c._name in names]

    def getfailedcollections(self):
        return [rep for rep in self.getreports("pytest_collectreport") if rep.failed]

    def getreports(self, names=("pytest_collectreport", "pytest_runtest_logreport")):
        return [c.report for c in self.getcalls(names) if hasattr(c, "report")]

    def assert_contains(self, entries):
        """Assert that recorded hook calls contain the given (name, expr) pairs in order."""
        __tracebackhide__ = True
        i = 0
        entries = list(entries)
        backlocals = dict(sys._getframe(1).f_locals)
        while entries:
            name, check = entries.pop(0)
            for ind, call in enumerate(self._hook_calls[i:]):
                if call._name == name:
                    if eval(check, backlocals, call.__dict__):  # noqa: S307
                        pass
                    else:
                        continue
                    i += ind + 1
                    break
            else:
                from pytest._outcomes import fail

                fail(f"could not find {name!r} check {check!r}")

    def listoutcomes(self):
        outcomes = {"passed": [], "skipped": [], "failed": []}
        seen = set()
        for line in self._result.outlines:
            parts = line.split()
            if len(parts) < 2:
                continue
            # Format 1: "nodeid WORD [progress]" — verbose run-time output
            # Format 2: "WORD nodeid - message" — short test summary info section
            if parts[0] in self._WORDS:
                # Short summary format: "FAILED nodeid - ..."
                word = parts[0]
                nodeid = parts[1]
            else:
                # Verbose format: "nodeid WORD [progress]"
                nodeid = parts[0]
                word = parts[1]

            bucket = self._WORDS.get(word)
            is_test_node = (
                "::" in nodeid
                or nodeid.endswith((".txt", ".rst", ".md"))
                or (nodeid.endswith(".py") and "." in nodeid.split("/")[-1][:-3])
            )
            if bucket is not None and is_test_node and nodeid not in seen:
                seen.add(nodeid)
                outcomes[bucket].append(_OutcomeReport(nodeid, bucket))
        # Collect-level reports (e.g. a skipped DoctestModule, a module that
        # failed to import) have no per-item lines; the final summary counts
        # are authoritative, so pad each bucket up to them. Upstream's
        # HookRecorder counts collect reports too: xpassed→passed,
        # xfailed→skipped, errors→failed.
        totals = self._result.parseoutcomes()
        expected = {
            "passed": totals.get("passed", 0) + totals.get("xpassed", 0),
            "skipped": totals.get("skipped", 0) + totals.get("xfailed", 0),
            "failed": totals.get("failed", 0) + totals.get("errors", 0),
        }
        for bucket, want in expected.items():
            while len(outcomes[bucket]) < want:
                outcomes[bucket].append(_OutcomeReport("<collect report>", bucket))
        return outcomes["passed"], outcomes["skipped"], outcomes["failed"]

    def _teardown_reports(self):
        """Failed teardown reports parsed from the "ERROR at teardown of X"
        failure sections."""
        text = "\n".join(self._result.outlines)
        return [
            _OutcomeReport(match.group(1), "failed", "teardown", match.group(2))
            for match in re.finditer(
                r"_{6,} ERROR at teardown of (.+?) _{6,}\n(.*?)(?=\n_{6,} |\n={6,} |\Z)",
                text,
                re.S,
            )
        ]

    def matchreport(self, inamepart="", when=None):
        """The single report whose nodeid's last part contains inamepart
        (HookRecorder.matchreport: call reports unless `when` says else)."""
        if when == "teardown":
            candidates = self._teardown_reports()
        else:
            passed, skipped, failed = self.listoutcomes()
            candidates = [*passed, *skipped, *failed]
        values = [
            rep for rep in candidates if not inamepart or inamepart in rep.nodeid.split("::")[-1]
        ]
        if not values:
            raise ValueError(f"could not find test report matching {inamepart!r}")
        if len(values) > 1:
            raise ValueError(f"found 2 or more testreports matching {inamepart!r}: {values}")
        return values[0]

    def assertoutcome(self, passed=0, skipped=0, failed=0):
        __tracebackhide__ = True
        got_passed, got_skipped, got_failed = self.listoutcomes()
        got = (len(got_passed), len(got_skipped), len(got_failed))
        assert got == (passed, skipped, failed), (
            f"assertoutcome: expected (passed={passed}, skipped={skipped}, "
            f"failed={failed}), got {got}:\n{self._result.stdout}"
        )

    def countoutcomes(self):
        return [len(outcome) for outcome in self.listoutcomes()]
