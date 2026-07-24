"""xdist-parity builtin fixtures: worker_id / testrun_uid.

Available in every run (like having pytest-xdist installed): "master" /
a fresh uid without -n, the worker's values under -n.
"""

import os
import uuid
import warnings

from pytest._fixtures import fixture

_FALLBACK_UID = uuid.uuid4().hex


@fixture
def worker_id():
    return os.environ.get("PYTEST_XDIST_WORKER", "master")


@fixture(scope="session")
def testrun_uid():
    return os.environ.get("PYTEST_XDIST_TESTRUNUID", _FALLBACK_UID)


def auto_num_workers(logical):
    """Resolve `-n auto` / `-n logical` like upstream xdist's default
    pytest_xdist_auto_num_workers impl: the PYTEST_XDIST_AUTO_NUM_WORKERS
    env override, then psutil if installed (the pytest-xdist[psutil]
    extra: physical cores for auto, logical for logical), then
    sched_getaffinity / cpu_count."""
    env_var = os.environ.get("PYTEST_XDIST_AUTO_NUM_WORKERS")
    if env_var:
        try:
            return int(env_var)
        except ValueError:
            warnings.warn(
                f"PYTEST_XDIST_AUTO_NUM_WORKERS is not a number: {env_var!r}. Ignoring it."
            )
    try:
        import psutil
    except ImportError:
        pass
    else:
        count = psutil.cpu_count(logical=logical) or psutil.cpu_count()
        if count:
            return count
    try:
        from os import sched_getaffinity
    except ImportError:
        pass
    else:
        return len(sched_getaffinity(0))
    return os.cpu_count() or 1


def set_worker_title(title):
    """Worker process title (the pytest-xdist[setproctitle] extra); a
    no-op when setproctitle is not installed, like upstream."""
    try:
        from setproctitle import setproctitle
    except ImportError:
        return
    try:
        setproctitle(title)
    except Exception:
        # changing the process name is very optional, no errors please
        pass
