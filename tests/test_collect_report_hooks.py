"""pytest_collectstart / pytest_make_collect_report / pytest_collectreport (#134).

Upstream calls them for every collector, depth-first while it collects:
collectstart, make_collect_report, the children, collectreport (whose
`result` is the collector's children). pytest-rs replays its native
collection through the shim pluginmanager in that order, once, so
conftests, plugins, a replacement terminal reporter and pytester's
HookRecorder each see every call exactly once. The expected sequences below
are pytest 9.2's.
"""

from __future__ import annotations

import os
import re
import subprocess
import tempfile
import textwrap
import unittest
from pathlib import Path

ROOT = Path(__file__).parent.parent
# PYTEST_RS_BIN selects a binary built for another Python (see conformance/runner.py).
BINARY = Path(os.environ.get("PYTEST_RS_BIN") or ROOT / "target" / "debug" / "pytest-rs-bin")

PLUGIN = """
def _d(c):
    return f"{type(c).__name__}:{c.nodeid!r}"


def pytest_collectstart(collector):
    print("CS", _d(collector))


def pytest_make_collect_report(collector):
    print("MR", _d(collector))


def pytest_collectreport(report):
    result = [f"{type(x).__name__}:{x.nodeid}" for x in report.result]
    print("CR", repr(report.nodeid), report.outcome, result)
"""


class TestCollectReportHooks(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        if not BINARY.exists():
            raise unittest.SkipTest("debug binary not built")

    def run_pytest(
        self, files: dict[str, str], *args: str, plugin: str = PLUGIN
    ) -> subprocess.CompletedProcess[str]:
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "proj"
            plugins = Path(tmp) / "plugins"
            plugins.mkdir()
            (plugins / "crh_plugin.py").write_text(textwrap.dedent(plugin))
            for name, text in files.items():
                (path / name).parent.mkdir(parents=True, exist_ok=True)
                (path / name).write_text(textwrap.dedent(text))
            env = dict(os.environ, PYTHONPATH=str(plugins))
            env.pop("PYTEST_ADDOPTS", None)
            result = subprocess.run(
                [str(BINARY), "-p", "crh_plugin", "-p", "no:cacheprovider", "-s", "-q", *args],
                cwd=path,
                capture_output=True,
                text=True,
                env=env,
            )
            result.files = {  # type: ignore[attr-defined]
                str(p.relative_to(path)): p.read_text() for p in path.rglob("*.xml")
            }
            return result

    @staticmethod
    def calls(result: subprocess.CompletedProcess[str]) -> list[str]:
        return re.findall(r"^(?:CS|MR|CR) .*$", result.stdout, re.M)

    def test_plain_module(self) -> None:
        result = self.run_pytest({"test_x.py": "def test_a(): pass\ndef test_b(): pass\n"})
        self.assertEqual(
            self.calls(result),
            [
                "CS Session:''",
                "MR Session:''",
                "CR '' passed ['Dir:.']",
                "CS Dir:'.'",
                "MR Dir:'.'",
                "CS Module:'test_x.py'",
                "MR Module:'test_x.py'",
                "CR 'test_x.py' passed ['Function:test_x.py::test_a', 'Function:test_x.py::test_b']",
                "CR '.' passed ['Module:test_x.py']",
            ],
            result.stdout,
        )

    def test_module_with_class_and_subdirs(self) -> None:
        result = self.run_pytest(
            {
                "test_a.py": """
                    def test_1(): pass
                    class TestK:
                        def test_m(self): pass
                """,
                "sub/test_b.py": """
                    import pytest
                    @pytest.mark.parametrize("x", [1, 2])
                    def test_p(x): pass
                """,
                "sub/pkg/__init__.py": "",
                "sub/pkg/test_c.py": "def test_c(): pass\n",
            }
        )
        self.assertEqual(
            self.calls(result),
            [
                "CS Session:''",
                "MR Session:''",
                "CR '' passed ['Dir:.']",
                "CS Dir:'.'",
                "MR Dir:'.'",
                "CS Dir:'sub'",
                "MR Dir:'sub'",
                "CS Package:'sub/pkg'",
                "MR Package:'sub/pkg'",
                "CS Module:'sub/pkg/test_c.py'",
                "MR Module:'sub/pkg/test_c.py'",
                "CR 'sub/pkg/test_c.py' passed ['Function:sub/pkg/test_c.py::test_c']",
                "CR 'sub/pkg' passed ['Module:sub/pkg/test_c.py']",
                "CS Module:'sub/test_b.py'",
                "MR Module:'sub/test_b.py'",
                "CR 'sub/test_b.py' passed"
                " ['Function:sub/test_b.py::test_p[1]', 'Function:sub/test_b.py::test_p[2]']",
                "CR 'sub' passed ['Package:sub/pkg', 'Module:sub/test_b.py']",
                "CS Module:'test_a.py'",
                "MR Module:'test_a.py'",
                "CS Class:'test_a.py::TestK'",
                "MR Class:'test_a.py::TestK'",
                "CR 'test_a.py::TestK' passed ['Function:test_a.py::TestK::test_m']",
                "CR 'test_a.py' passed ['Function:test_a.py::test_1', 'Class:test_a.py::TestK']",
                "CR '.' passed ['Dir:sub', 'Module:test_a.py']",
            ],
            result.stdout,
        )
        self.assertIn("5 passed", result.stdout)

    def test_arguments(self) -> None:
        """The Session's report lists what the arguments matched, after the
        collectors on the way to them were collected (never reported)."""
        result = self.run_pytest(
            {
                "test_a.py": "class TestK:\n    def test_m(self): pass\n",
                "sub/test_b.py": "def test_b(): pass\n",
            },
            "sub/test_b.py",
            "test_a.py::TestK",
        )
        self.assertEqual(
            self.calls(result),
            [
                "CS Session:''",
                "MR Session:''",
                "CS Dir:'.'",
                "MR Dir:'.'",
                "CS Dir:'sub'",
                "MR Dir:'sub'",
                "CS Module:'test_a.py'",
                "MR Module:'test_a.py'",
                "CR '' passed ['Module:sub/test_b.py', 'Class:test_a.py::TestK']",
                "CS Module:'sub/test_b.py'",
                "MR Module:'sub/test_b.py'",
                "CR 'sub/test_b.py' passed ['Function:sub/test_b.py::test_b']",
                "CS Class:'test_a.py::TestK'",
                "MR Class:'test_a.py::TestK'",
                "CR 'test_a.py::TestK' passed ['Function:test_a.py::TestK::test_m']",
            ],
            result.stdout,
        )

    def test_collection_error_and_skipped_module(self) -> None:
        """A failed or skipped module's report is delivered once, even when
        the collection errors abort the session."""
        files = {
            "test_err.py": "import nonexistent_module_xyz\n",
            "test_ok.py": "def test_a(): pass\n",
            "test_skip.py": "import pytest\npytest.skip('nope', allow_module_level=True)\n",
        }
        expected = [
            "CS Session:''",
            "MR Session:''",
            "CR '' passed ['Dir:.']",
            "CS Dir:'.'",
            "MR Dir:'.'",
            "CS Module:'test_err.py'",
            "MR Module:'test_err.py'",
            "CR 'test_err.py' failed []",
            "CS Module:'test_ok.py'",
            "MR Module:'test_ok.py'",
            "CR 'test_ok.py' passed ['Function:test_ok.py::test_a']",
            "CS Module:'test_skip.py'",
            "MR Module:'test_skip.py'",
            "CR 'test_skip.py' skipped []",
            "CR '.' passed ['Module:test_err.py', 'Module:test_ok.py', 'Module:test_skip.py']",
        ]
        for args in ([], ["--continue-on-collection-errors"]):
            result = self.run_pytest(files, *args)
            self.assertEqual(self.calls(result), expected, result.stdout)

    def test_hook_exception_is_internal_error(self) -> None:
        result = self.run_pytest(
            {"test_x.py": "def test_a(): pass\n"},
            plugin="def pytest_collectreport(report):\n    raise ValueError('boom')\n",
        )
        self.assertEqual(result.returncode, 3, result.stdout + result.stderr)
        self.assertIn("INTERNALERROR> ValueError: boom", result.stdout)

    def test_replacement_reporter_counts_once(self) -> None:
        """A replacement terminal reporter (pytest-sugar, pytest-instafail)
        counts the items of the collectreports it receives: no double count,
        and the collection error reaches it once."""
        plugin = """
            import pytest
            from _pytest.terminal import TerminalReporter

            class Reporter(TerminalReporter):
                def pytest_collectreport(self, report):
                    super().pytest_collectreport(report)
                    if report.failed:
                        print("FAILED-REPORT", report.nodeid)

                def pytest_collection_finish(self, session):
                    super().pytest_collection_finish(session)
                    print("NUMCOLLECTED", self._numcollected)

            @pytest.hookimpl(trylast=True)
            def pytest_configure(config):
                standard = config.pluginmanager.getplugin("terminalreporter")
                config.pluginmanager.unregister(standard)
                config.pluginmanager.register(Reporter(config), "terminalreporter")
        """
        result = self.run_pytest(
            {
                "test_a.py": "def test_1(): pass\nclass TestK:\n    def test_m(self): pass\n",
                "sub/test_b.py": "def test_b(): pass\n",
                "test_err.py": "import nonexistent_module_xyz\n",
            },
            "--continue-on-collection-errors",
            "-v",
            plugin=plugin,
        )
        self.assertIn("NUMCOLLECTED 3", result.stdout)
        self.assertEqual(result.stdout.count("FAILED-REPORT test_err.py"), 1, result.stdout)
        self.assertIn("collected 3 items / 1 error", result.stdout.replace("\r", ""))

    def test_junitxml_collection_error_once(self) -> None:
        result = self.run_pytest(
            {
                "test_err.py": "import nonexistent_module_xyz\n",
                "test_ok.py": "def test_a(): pass\n",
            },
            "--junitxml=out.xml",
        )
        xml = result.files["out.xml"]  # type: ignore[attr-defined]
        self.assertEqual(xml.count("<error "), 1, xml)
        self.assertEqual(self.calls(result).count("CR 'test_err.py' failed []"), 1)

    def test_collectstart_class_markers_reach_items(self) -> None:
        """pytest-django tags a Class collector in pytest_collectstart; the
        markers apply to the class's items."""
        plugin = """
            import pytest

            def pytest_collectstart(collector):
                if isinstance(collector, pytest.Class) and getattr(collector.obj, "tags", ()):
                    for tag in collector.obj.tags:
                        collector.add_marker(tag)
        """
        result = self.run_pytest(
            {
                "test_t.py": """
                    class TestTagged:
                        tags = ["fast"]
                        def test_m(self): pass
                    class TestPlain:
                        def test_n(self): pass
                """,
                "pytest.ini": "[pytest]\nmarkers =\n    fast: tagged\n",
            },
            "-m",
            "fast",
            plugin=plugin,
        )
        self.assertIn("1 passed, 1 deselected", result.stdout)

    def test_hookrecorder_counts(self) -> None:
        """pytester's HookRecorder sees each call once."""
        files = {
            "conftest.py": "pytest_plugins = ['pytester']\n",
            "test_rec.py": """
                def test_rec(pytester):
                    pytester.makepyfile(
                        test_one='''
                            def test_one(): pass
                            class TestX:
                                def test_method_one(self): pass
                        ''',
                        test_three="xxxdsadsadsadsa",
                    )
                    reprec = pytester.inline_run("--collect-only", pytester.path)
                    started = reprec.getcalls("pytest_collectstart")
                    finished = reprec.getreports("pytest_collectreport")
                    print("STARTED", len(started), "FINISHED", len(finished))
                    print("MAKEREPORT", len(reprec.getcalls("pytest_make_collect_report")))
                    print("FAILED", len([x for x in finished if x.failed]))
                    names = [c.name for c in reprec.getcalls("pytest_pycollect_makeitem")]
                    # Upstream also calls it for the module's other names.
                    print(
                        "MAKEITEM",
                        sorted(n for n in names if n in ("TestX", "test_method_one", "test_one")),
                    )
            """,
        }
        result = self.run_pytest(files, "-p", "pytester", plugin="")
        # Session, Dir, test_one.py, TestX, test_three.py.
        self.assertIn("STARTED 5 FINISHED 5", result.stdout, result.stdout + result.stderr)
        self.assertIn("MAKEREPORT 5", result.stdout)
        self.assertIn("FAILED 1", result.stdout)
        self.assertIn("MAKEITEM ['TestX', 'test_method_one', 'test_one']", result.stdout)


if __name__ == "__main__":
    unittest.main()
