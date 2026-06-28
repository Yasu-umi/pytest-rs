import sys

from pytest import LineMatcher, Pytester, RunResult  # noqa: F401

# Register pytester_assertions for assertion rewriting before it is first
# imported.  Upstream relies on the pytest11 entry point; we do it here since
# pytester.py is always loaded before any test calls assertoutcome().
try:
    from pytest import _rewrite as _rewrite_mod
    _rewrite_mod.register_assert_rewrite("_pytest.pytester_assertions")
except Exception:
    pass


class SysModulesSnapshot:
    """Snapshot of ``sys.modules`` that ``restore()`` reinstates in place
    (the live mapping object is preserved, only its contents are reset)."""

    def __init__(self, preserve=None):
        self.__saved = dict(sys.modules)
        self.__preserve = preserve

    def restore(self):
        if self.__preserve:
            self.__saved.update((k, m) for k, m in sys.modules.items() if self.__preserve(k))
        sys.modules.clear()
        sys.modules.update(self.__saved)


class SysPathsSnapshot:
    """Snapshot of ``sys.path``/``sys.meta_path`` restored by in-place slice
    assignment, so the live list objects are preserved."""

    def __init__(self):
        self.__saved = list(sys.path), list(sys.meta_path)

    def restore(self):
        sys.path[:], sys.meta_path[:] = self.__saved


from pytest._outcomes import fail  # noqa: E402


class RecordedHookCall:
    """A recorded hook call; the hook's kwargs are set as attributes."""

    def __init__(self, name, kwargs):
        self.__dict__.update(kwargs)
        self._name = name

    def __repr__(self):
        d = self.__dict__.copy()
        del d["_name"]
        return f"<RecordedHookCall {self._name!r}(**{d!r})>"


# Upstream exposes the recorded call under this name in some versions.
ParsedCall = RecordedHookCall


class HookRecorder:
    """Record every hook called through a plugin manager (port of upstream
    pytester.HookRecorder), so tests can introspect what fired."""

    def __init__(self, pluginmanager, *, _ispytest=False):
        self._pluginmanager = pluginmanager
        self.calls = []
        self.ret = None

        def before(hook_name, hook_impls, kwargs):
            self.calls.append(RecordedHookCall(hook_name, kwargs))

        def after(outcome, hook_name, hook_impls, kwargs):
            pass

        self._undo_wrapping = pluginmanager.add_hookcall_monitoring(before, after)

    def finish_recording(self):
        self._undo_wrapping()

    def getcalls(self, names):
        if isinstance(names, str):
            names = names.split()
        return [call for call in self.calls if call._name in names]

    def assert_contains(self, entries):
        __tracebackhide__ = True
        i = 0
        entries = list(entries)
        backlocals = dict(sys._getframe(1).f_locals)
        while entries:
            name, check = entries.pop(0)
            for ind, call in enumerate(self.calls[i:]):
                if call._name == name:
                    if eval(check, backlocals, call.__dict__):
                        pass
                    else:
                        continue
                    i += ind + 1
                    break
            else:
                fail(f"could not find {name!r} check {check!r}")

    def popcall(self, name):
        __tracebackhide__ = True
        for i, call in enumerate(self.calls):
            if call._name == name:
                del self.calls[i]
                return call
        lines = [f"could not find call {name!r}, in:"]
        lines.extend([f"  {x}" for x in self.calls])
        fail("\n".join(lines))

    def getcall(self, name):
        values = self.getcalls(name)
        assert len(values) == 1, (name, values)
        return values[0]

    def getreports(self, names=("pytest_collectreport", "pytest_runtest_logreport")):
        return [x.report for x in self.getcalls(names)]

    def matchreport(
        self,
        inamepart="",
        names=("pytest_runtest_logreport", "pytest_collectreport"),
        when=None,
    ):
        values = []
        for rep in self.getreports(names=names):
            if not when and rep.when != "call" and rep.passed:
                continue
            if when and rep.when != when:
                continue
            if not inamepart or inamepart in rep.nodeid.split("::"):
                values.append(rep)
        if not values:
            raise ValueError(
                f"could not find test report matching {inamepart!r}: no test reports at all!"
            )
        if len(values) > 1:
            raise ValueError(f"found 2 or more testreports matching {inamepart!r}: {values}")
        return values[0]

    def getfailures(self, names=("pytest_collectreport", "pytest_runtest_logreport")):
        return [rep for rep in self.getreports(names) if rep.failed]

    def getfailedcollections(self):
        return self.getfailures("pytest_collectreport")

    def listoutcomes(self):
        passed = []
        skipped = []
        failed = []
        for rep in self.getreports(("pytest_collectreport", "pytest_runtest_logreport")):
            if rep.passed:
                if rep.when == "call":
                    passed.append(rep)
            elif rep.skipped:
                skipped.append(rep)
            else:
                assert rep.failed, f"Unexpected outcome: {rep!r}"
                failed.append(rep)
        return passed, skipped, failed

    def countoutcomes(self):
        return [len(x) for x in self.listoutcomes()]

    def assertoutcome(self, passed=0, skipped=0, failed=0):
        __tracebackhide__ = True
        from _pytest.pytester_assertions import assertoutcome
        outcomes = self.listoutcomes()
        assertoutcome(outcomes, passed=passed, skipped=skipped, failed=failed)

    def clear(self):
        self.calls[:] = []


def pytest_addoption(parser):
    """Register pytester CLI options so -p pytester + --runpytest work."""
    parser.addoption(
        "--lsof",
        action="store_true",
        dest="lsof",
        default=False,
        help="Run FD checks if lsof is available",
    )
    parser.addoption(
        "--runpytest",
        default="inprocess",
        dest="runpytest",
        choices=("inprocess", "subprocess"),
        help="Run pytest sub runs via 'inprocess' or 'subprocess'",
    )


def get_public_names(values):
    """Only return names from iterator values without a leading underscore."""
    return [x for x in values if x[0] != "_"]


from _pytest._stub import __getattr__  # noqa: E402, F401
