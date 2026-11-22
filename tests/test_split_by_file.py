"""Regression test: pytest-split's group assignment, with and without --split-by-file.

The default --splits/--group assignment is per test and happens after
collection; the expected groups below were recorded with the build before
--split-by-file existed and must not change. With --split-by-file, whole
files are assigned before import: each group (and each -n worker of it)
imports only its own files, and the groups still partition the suite.
See docs/split-by-file.md.
"""

from __future__ import annotations

import json
import os
import subprocess
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).parent.parent
# PYTEST_RS_BIN selects a binary built for another Python (see conformance/runner.py).
BINARY = Path(os.environ.get("PYTEST_RS_BIN") or ROOT / "target" / "debug" / "pytest-rs-bin")

MODULE = """\
import os
with open(os.environ["IMPORT_LOG"], "a") as f:
    f.write(__name__ + "\\n")
import pytest
@pytest.mark.parametrize("n", range({n}))
def test_a(n):
    pass
def test_b():
    pass
"""

# test_m{i}.py has i parametrized test_a cases of 0.5*i s each and a 0.1 s
# test_b; sub/test_s.py (two test_a cases) has no recorded durations.
FILES = {f"test_m{i}.py": i for i in range(1, 6)} | {"sub/test_s.py": 2}
DURATIONS = {
    nodeid: duration
    for i in range(1, 6)
    for nodeid, duration in [
        *((f"test_m{i}.py::test_a[{n}]", 0.5 * i) for n in range(i)),
        (f"test_m{i}.py::test_b", 0.1),
    ]
}

# Recorded before --split-by-file existed (per-test assignment).
DEFAULT = {
    ("duration_based_chunks", 1): "sub/test_s.py::test_a[0] sub/test_s.py::test_a[1] "
    "sub/test_s.py::test_b test_m1.py::test_a[0] test_m1.py::test_b test_m2.py::test_a[0] "
    "test_m2.py::test_a[1] test_m2.py::test_b test_m3.py::test_a[0] test_m3.py::test_a[1] "
    "test_m3.py::test_a[2]",
    ("duration_based_chunks", 2): "test_m3.py::test_b test_m4.py::test_a[0] "
    "test_m4.py::test_a[1] test_m4.py::test_a[2] test_m4.py::test_a[3] test_m4.py::test_b "
    "test_m5.py::test_a[0] test_m5.py::test_a[1]",
    ("duration_based_chunks", 3): "test_m5.py::test_a[2] test_m5.py::test_a[3] "
    "test_m5.py::test_a[4] test_m5.py::test_b",
    ("least_duration", 1): "sub/test_s.py::test_a[1] test_m2.py::test_a[1] "
    "test_m3.py::test_a[1] test_m4.py::test_a[2] test_m5.py::test_a[0] test_m5.py::test_a[3]",
    ("least_duration", 2): "sub/test_s.py::test_b test_m1.py::test_a[0] test_m1.py::test_b "
    "test_m3.py::test_a[2] test_m3.py::test_b test_m4.py::test_a[3] test_m5.py::test_a[1] "
    "test_m5.py::test_a[4] test_m5.py::test_b",
    ("least_duration", 3): "sub/test_s.py::test_a[0] test_m2.py::test_a[0] test_m2.py::test_b "
    "test_m3.py::test_a[0] test_m4.py::test_a[0] test_m4.py::test_a[1] test_m4.py::test_b "
    "test_m5.py::test_a[2]",
}

# --split-by-file. File durations: m1 0.6, m2 2.1, m3 4.6, m4 8.1, m5 12.6,
# sub/test_s.py the average of those, 5.6. least_duration places the longest
# file first into the shortest group; duration_based_chunks cuts the files in
# collection order, which with few long files can leave a group empty.
BY_FILE = {
    ("duration_based_chunks", 1): {"sub/test_s.py", "test_m1.py", "test_m2.py", "test_m3.py"},
    ("duration_based_chunks", 2): {"test_m4.py", "test_m5.py"},
    ("duration_based_chunks", 3): set(),
    ("least_duration", 1): {"test_m5.py"},
    ("least_duration", 2): {"test_m1.py", "test_m2.py", "test_m4.py"},
    ("least_duration", 3): {"sub/test_s.py", "test_m3.py"},
}


class TestSplitByFile(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        if not BINARY.exists():
            raise unittest.SkipTest("debug binary not built")

    def _run(self, *args: str) -> tuple[list[str], list[str]]:
        """(passed node ids, imported test module names) of one run."""
        with tempfile.TemporaryDirectory() as tmp:
            tmp_path = Path(tmp)
            for name, n in FILES.items():
                (tmp_path / name).parent.mkdir(exist_ok=True)
                (tmp_path / name).write_text(MODULE.format(n=n))
            (tmp_path / ".test_durations").write_text(json.dumps(DURATIONS))
            log = tmp_path / "imports.log"
            log.touch()
            result = subprocess.run(
                [str(BINARY), "-p", "no:cacheprovider", "-v", *args],
                cwd=tmp_path,
                capture_output=True,
                text=True,
                env={**os.environ, "IMPORT_LOG": str(log)},
            )
            self.assertIn(result.returncode, (0, 5), result.stdout + result.stderr)
            # "nodeid PASSED [ n%]" serially, "[gw0] [ n%] PASSED nodeid" under -n.
            passed = sorted(
                word
                for line in result.stdout.splitlines()
                if "PASSED" in line
                for word in line.split()
                if "::" in word
            )
            return passed, log.read_text().splitlines()

    @staticmethod
    def _all_tests(files: set[str]) -> list[str]:
        return sorted(
            nodeid
            for name in files
            for nodeid in [
                *(f"{name}::test_a[{n}]" for n in range(FILES[name])),
                f"{name}::test_b",
            ]
        )

    def test_default_assignment_unchanged(self) -> None:
        for (algorithm, group), expected in DEFAULT.items():
            with self.subTest(algorithm=algorithm, group=group):
                passed, imported = self._run(
                    "--splits", "3", "--group", str(group), "--splitting-algorithm", algorithm
                )
                self.assertEqual(passed, sorted(expected.split()))
                # The default still imports every file.
                self.assertEqual(len(imported), len(FILES))

    def test_split_by_file(self) -> None:
        for dist in ((), ("-n", "2")):
            for (algorithm, group), files in BY_FILE.items():
                with self.subTest(dist=dist, algorithm=algorithm, group=group):
                    passed, imported = self._run(
                        "--splits",
                        "3",
                        "--group",
                        str(group),
                        "--splitting-algorithm",
                        algorithm,
                        "--split-by-file",
                        *dist,
                    )
                    self.assertEqual(passed, self._all_tests(files))
                    modules = {Path(name).stem for name in files}
                    # Only the group's own files are imported, once per process.
                    self.assertEqual(set(imported), modules)
                    self.assertEqual(len(imported), len(files) * (2 if dist else 1))


if __name__ == "__main__":
    unittest.main()
