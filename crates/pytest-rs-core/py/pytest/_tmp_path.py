"""tmp_path / tmp_path_factory builtin fixtures (upstream _pytest/tmpdir.py
port: pytest-of-{user} numbered basetemps with lock-driven retention)."""

import getpass
import os
import pathlib
import re
import shutil
import stat
import tempfile
import time

from pytest._fixtures import fixture

_BASETEMP_PREFIX = "pytest-rs-basetemp-"
# Pre-numbered-dir releases used unique mkdtemp basetemps; killed sessions
# leaked them unboundedly. Keep sweeping those legacy leftovers for a while.
_STALE_SECONDS = 3 * 60 * 60

LOCK_TIMEOUT = 60 * 60 * 24

# --basetemp / retention ini values, set by the engine at startup.
_given_basetemp = None
_retention_count = 3
_retention_policy = "all"

# Call-phase outcomes per nodeid (engine-fed; upstream stores rep.passed in
# the item stash) and whether anything failed (upstream checks exitstatus).
_call_results: dict = {}
_any_failed = False


def configure(basetemp, retention_count=None, retention_policy=None):
    global _given_basetemp, _retention_count, _retention_policy, _any_failed
    _given_basetemp = basetemp
    if retention_count is not None:
        _retention_count = int(retention_count)
    if retention_policy is not None:
        _retention_policy = retention_policy
    # Each session starts retention bookkeeping fresh; otherwise an in-process
    # nested run would inherit the outer run's pass/fail outcomes and prune the
    # wrong tmp dirs (tmp_path_retention_policy="failed").
    _call_results.clear()
    _any_failed = False


def record_call(nodeid, passed):
    """The engine reports each item's call outcome (None: no call ran)
    before function-scope finalizers — the tmp_path retention teardown
    reads it (upstream's tmppath_result_key stash)."""
    global _any_failed
    if passed is not None:
        _call_results[nodeid] = passed
        if not passed:
            _any_failed = True


def rm_rf(path):
    """Best-effort _pytest.pathlib.rm_rf (engine teardown callers must not
    raise on a half-removed tree)."""
    from _pytest.pathlib import rm_rf as _rm_rf

    try:
        _rm_rf(path)
    except OSError:
        pass


def _sweep_stale_basetemps():
    """Best-effort removal of legacy mkdtemp basetemps left by old builds."""
    cutoff = time.time() - _STALE_SECONDS
    try:
        entries = list(pathlib.Path(tempfile.gettempdir()).iterdir())
    except OSError:
        return
    for entry in entries:
        if not entry.name.startswith(_BASETEMP_PREFIX):
            continue
        try:
            if entry.stat().st_mtime < cutoff:
                rm_rf(entry)
        except OSError:
            continue


def get_user():
    """Return the current user name, or None if getuser() does not work
    in the current environment (see #1010)."""
    try:
        # In some exotic environments, getpass may not be importable.
        return getpass.getuser()
    except (ImportError, OSError, KeyError):
        return None


def _check_ispytest(ispytest):
    from _pytest.deprecated import check_ispytest

    check_ispytest(ispytest)


class TempPathFactory:
    """Factory for temporary directories under the common base temp
    directory (upstream TempPathFactory)."""

    def __init__(
        self,
        given_basetemp,
        retention_count,
        retention_policy,
        trace,
        basetemp=None,
        *,
        _ispytest=False,
    ):
        _check_ispytest(_ispytest)
        if given_basetemp is None:
            self._given_basetemp = None
        else:
            # Use os.path.abspath() to get absolute path instead of resolve()
            # as it does not work the same in all platforms (see #4427).
            self._given_basetemp = pathlib.Path(os.path.abspath(str(given_basetemp)))
        self._trace = trace
        self._retention_count = retention_count
        self._retention_policy = retention_policy
        self._basetemp = basetemp

    @classmethod
    def from_config(cls, config, *, _ispytest=False):
        """Create a factory according to pytest configuration."""
        _check_ispytest(_ispytest)
        count = int(config.getini("tmp_path_retention_count"))
        if count < 0:
            raise ValueError(f"tmp_path_retention_count must be >= 0. Current input: {count}.")

        policy = config.getini("tmp_path_retention_policy")
        if policy not in ("all", "failed", "none"):
            raise ValueError(
                f"tmp_path_retention_policy must be either all, failed, none. "
                f"Current input: {policy}."
            )

        return cls(
            given_basetemp=config.option.basetemp,
            trace=config.trace.get("tmpdir"),
            retention_count=count,
            retention_policy=policy,
            _ispytest=True,
        )

    def _ensure_relative_to_basetemp(self, basename):
        basename = os.path.normpath(basename)
        if (self.getbasetemp() / basename).resolve().parent != self.getbasetemp():
            raise ValueError(f"{basename} is not a normalized and relative path")
        return basename

    def mktemp(self, basename, numbered=True):
        """Create a new temporary directory managed by the factory."""
        from _pytest.pathlib import make_numbered_dir

        basename = self._ensure_relative_to_basetemp(str(basename))
        if not numbered:
            p = self.getbasetemp().joinpath(basename)
            p.mkdir(mode=0o700)
        else:
            p = make_numbered_dir(root=self.getbasetemp(), prefix=basename, mode=0o700)
            self._trace("mktemp", p)
        return p

    def getbasetemp(self):
        """Return the base temporary directory, creating it if needed."""
        from _pytest.compat import get_user_id
        from _pytest.pathlib import make_numbered_dir_with_cleanup, rm_rf

        if self._basetemp is not None:
            return self._basetemp

        if self._given_basetemp is not None:
            basetemp = self._given_basetemp
            if basetemp.exists():
                rm_rf(basetemp)
            basetemp.mkdir(mode=0o700, parents=True, exist_ok=True)
            basetemp = basetemp.resolve()
        else:
            _sweep_stale_basetemps()
            from_env = os.environ.get("PYTEST_DEBUG_TEMPROOT")
            temproot = pathlib.Path(from_env or tempfile.gettempdir()).resolve()
            user = get_user() or "unknown"
            # use a sub-directory in the temproot to speed-up
            # make_numbered_dir() call
            rootdir = temproot.joinpath(f"pytest-of-{user}")
            try:
                rootdir.mkdir(mode=0o700, exist_ok=True)
            except OSError:
                # getuser() likely returned illegal characters for the
                # platform, use unknown back off mechanism
                rootdir = temproot.joinpath("pytest-of-unknown")
                rootdir.mkdir(mode=0o700, exist_ok=True)
            # Because we use exist_ok=True with a predictable name, make sure
            # we are the owners, to prevent any funny business (on unix, where
            # temproot is usually shared). Also fixup any world-readable temp
            # rootdir's permissions, and reject symlinked rootdirs
            # (CVE-2025-71176).
            uid = get_user_id()
            if uid is not None:
                stat_follow_symlinks = False if os.stat in os.supports_follow_symlinks else True
                rootdir_stat = rootdir.stat(follow_symlinks=stat_follow_symlinks)
                if stat.S_ISLNK(rootdir_stat.st_mode):
                    raise OSError(
                        f"The temporary directory {rootdir} is a symbolic link. "
                        "Fix this and try again."
                    )
                if rootdir_stat.st_uid != uid:
                    raise OSError(
                        f"The temporary directory {rootdir} is not owned by the current user. "
                        "Fix this and try again."
                    )
                if (rootdir_stat.st_mode & 0o077) != 0:
                    chmod_follow_symlinks = (
                        False if os.chmod in os.supports_follow_symlinks else True
                    )
                    rootdir.chmod(
                        rootdir_stat.st_mode & ~0o077,
                        follow_symlinks=chmod_follow_symlinks,
                    )
            keep = self._retention_count
            if self._retention_policy == "none":
                keep = 0
            basetemp = make_numbered_dir_with_cleanup(
                prefix="pytest-",
                root=rootdir,
                keep=keep,
                lock_timeout=LOCK_TIMEOUT,
                mode=0o700,
            )
        assert basetemp is not None, basetemp
        self._basetemp = basetemp
        self._trace("new basetemp", basetemp)
        return basetemp


@fixture(scope="session")
def tmp_path_factory():
    """Return a :class:`pytest.TempPathFactory` instance for the test session."""
    factory = TempPathFactory(
        given_basetemp=_given_basetemp,
        retention_count=_retention_count,
        retention_policy=_retention_policy,
        trace=lambda *args: None,
        _ispytest=True,
    )
    yield factory
    # Upstream pytest_sessionfinish: a fully-passed run under the "failed"
    # policy removes the whole basetemp. ("all" keeps it; the numbered-dir
    # retention prunes old ones at the next session's creation.)
    if (
        factory._basetemp is not None
        and factory._given_basetemp is None
        and factory._retention_policy == "failed"
        and not _any_failed
    ):
        rm_rf(factory._basetemp)


@fixture
def tmp_path(tmp_path_factory, request):
    """Return a temporary directory (as :class:`pathlib.Path` object)
    which is unique to each test function invocation.
    The temporary directory is created as a subdirectory
    of the base temporary directory, with configurable retention,
    as discussed in :ref:`temporary directory location and retention`.
    """
    name = re.sub(r"\W", "_", request.node.name)[:30]
    path = tmp_path_factory.mktemp(name, numbered=True)
    yield path
    # Upstream: the "failed" policy removes the dir when the call passed
    # (or never ran — a setup skip counts as passed, issue #10502).
    if tmp_path_factory._retention_policy == "failed" and _call_results.pop(
        request.node.nodeid, True
    ):
        rm_rf(path)


class LocalPath:
    """Minimal py.path.local equivalent backing the legacy tmpdir fixture."""

    def __init__(self, path):
        self._path = pathlib.Path(path)

    def __str__(self):
        return str(self._path)

    def __repr__(self):
        return f"local({str(self._path)!r})"

    def __fspath__(self):
        return str(self._path)

    def __truediv__(self, other):
        return LocalPath(self._path / str(other))

    def __eq__(self, other):
        return str(self) == str(other)

    def __hash__(self):
        return hash(str(self._path))

    @property
    def strpath(self):
        return str(self._path)

    @property
    def basename(self):
        return self._path.name

    @property
    def dirname(self):
        return str(self._path.parent)

    def join(self, *parts):
        return LocalPath(self._path.joinpath(*[str(part) for part in parts]))

    def dirpath(self, *parts):
        return LocalPath(self._path.parent.joinpath(*[str(part) for part in parts]))

    def ensure(self, *parts, dir=False):
        target = self._path.joinpath(*[str(part) for part in parts])
        if dir:
            target.mkdir(parents=True, exist_ok=True)
        else:
            target.parent.mkdir(parents=True, exist_ok=True)
            target.touch()
        return LocalPath(target)

    def mkdir(self, *parts):
        target = self._path.joinpath(*[str(part) for part in parts])
        target.mkdir(parents=True)
        return LocalPath(target)

    def exists(self):
        return self._path.exists()

    def check(self, **kwargs):
        """py.path.local.check: bare = exists, kwargs assert properties
        (dir=1, file=1, exists=1; 0 negates)."""
        if not kwargs:
            return self._path.exists()
        probes = {
            "dir": self._path.is_dir,
            "file": self._path.is_file,
            "exists": self._path.exists,
        }
        for key, expected in kwargs.items():
            probe = probes.get(key)
            if probe is None or bool(probe()) != bool(expected):
                return False
        return True

    def isfile(self):
        return self._path.is_file()

    def isdir(self):
        return self._path.is_dir()

    def remove(self, ignore_errors=False):
        if self._path.is_dir():
            shutil.rmtree(self._path, ignore_errors=ignore_errors)
        else:
            self._path.unlink()

    def open(self, mode="r", encoding=None):
        return open(self._path, mode, encoding=encoding)

    def write(self, data, mode="w"):
        with open(self._path, mode) as f:
            f.write(data)
        return self

    def write_text(self, data, encoding="utf-8"):
        self._path.write_text(data, encoding=encoding)

    def read(self, mode="r"):
        with open(self._path, mode) as f:
            return f.read()

    def read_text(self, encoding="utf-8"):
        return self._path.read_text(encoding=encoding)

    def listdir(self):
        return [LocalPath(child) for child in sorted(self._path.iterdir())]

    def chdir(self):
        old = os.getcwd()
        os.chdir(self._path)
        return LocalPath(old)


@fixture
def tmpdir(tmp_path):
    return LocalPath(tmp_path)
