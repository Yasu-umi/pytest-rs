"""xdist-parity builtin fixtures: worker_id / testrun_uid.

Available in every run (like having pytest-xdist installed): "master" /
a fresh uid without -n, the worker's values under -n.
"""

import os
import uuid

from pytest._fixtures import fixture

_FALLBACK_UID = uuid.uuid4().hex


@fixture
def worker_id():
    return os.environ.get("PYTEST_XDIST_WORKER", "master")


@fixture(scope="session")
def testrun_uid():
    return os.environ.get("PYTEST_XDIST_TESTRUNUID", _FALLBACK_UID)
