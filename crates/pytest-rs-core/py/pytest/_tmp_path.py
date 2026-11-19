"""tmp_path / tmp_path_factory builtin fixtures (upstream _pytest/tmpdir.py
port: pytest-of-{user} numbered basetemps with lock-driven retention)."""

import atexit
import contextlib
import fnmatch
import getpass
import hashlib
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
# the item stash).
_call_results: dict = {}


# The interpreter's own atexit functions, captured at import: a test that
# swaps atexit.register out (pytest-xdist's conftest diverts it and runs the
# registered functions when the test ends) must not get the session's
# basetemp cleanup, or the basetemp loses its lock mid-session and a
# concurrent pytest process prunes it. Upstream looks atexit.register up at
# call time; it is the same function otherwise.
_atexit_register = atexit.register
_atexit_unregister = atexit.unregister


def configure(basetemp, retention_count=None, retention_policy=None):
    global _given_basetemp, _retention_count, _retention_policy
    _given_basetemp = basetemp
    if retention_count is not None:
        _retention_count = int(retention_count)
    if retention_policy is not None:
        _retention_policy = retention_policy
    # Each session starts retention bookkeeping fresh; otherwise an in-process
    # nested run would inherit the outer run's pass/fail outcomes and prune the
    # wrong tmp dirs (tmp_path_retention_policy="failed").
    _call_results.clear()


def record_call(nodeid, passed):
    """The engine reports each item's call outcome (None: no call ran)
    before function-scope finalizers — the tmp_path retention teardown
    reads it (upstream's tmppath_result_key stash)."""
    if passed is not None:
        _call_results[nodeid] = passed


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
        # Lower-cased prefix -> next number to try, so numbered dirs don't
        # need basetemp listed on every call.
        self._next_number = {}
        # Numbered-dir pruning and lock removal run when the session
        # finishes (pytest 9.2); atexit closes it only as a last resort.
        self._exit_stack = contextlib.ExitStack()

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
        basename = self._ensure_relative_to_basetemp(str(basename))
        if not numbered:
            p = self.getbasetemp().joinpath(basename)
            p.mkdir(mode=0o700)
        else:
            p = self._make_counted_dir(basename)
            self._trace("mktemp", p)
        return p

    def _make_counted_dir(self, prefix):
        """Create ``<prefix><n>`` in basetemp from a per-prefix counter
        (pytest 9.2): counting starts at 0 without listing basetemp, names
        mkdir fails on are skipped, the 10th failure lists basetemp once to
        jump past the run, and the 20th raises OSError."""
        # Looked up on _pytest.tmpdir, like upstream's module globals, so
        # tests monkeypatching _pytest.tmpdir.find_suffixes see it used.
        import _pytest.tmpdir as _tmpdir

        root = self.getbasetemp()
        key = prefix.lower()
        number = self._next_number.get(key, 0)
        collisions = 0
        while True:
            path = root.joinpath(f"{prefix}{number}")
            try:
                path.mkdir(mode=0o700)
            except OSError as e:
                collisions += 1
                if collisions == 10:
                    listed = max(
                        map(_tmpdir.parse_num, _tmpdir.find_suffixes(root, prefix)), default=-1
                    )
                    number = max(number, listed) + 1
                elif collisions == 20:
                    raise OSError(
                        f"could not create numbered dir with prefix {prefix} in {root}"
                        f" after {collisions} tries"
                    ) from e
                else:
                    number += 1
            else:
                break
        self._next_number[key] = number + 1
        _tmpdir._force_symlink(root, prefix + "current", path)
        return path

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
                register=self._exit_stack.callback,
            )
            _atexit_register(self._exit_stack.close)
            self._exit_stack.callback(_atexit_unregister, self._exit_stack.close)
        assert basetemp is not None, basetemp
        self._basetemp = basetemp
        self._trace("new basetemp", basetemp)
        return basetemp


@fixture(scope="session")
def tmp_path_factory(request):
    """Return a :class:`pytest.TempPathFactory` instance for the test session."""
    factory = TempPathFactory(
        given_basetemp=_given_basetemp,
        retention_count=_retention_count,
        retention_policy=_retention_policy,
        trace=lambda *args: None,
        _ispytest=True,
    )
    # Like upstream's config._tmp_path_factory: session_finish() below (the
    # tmpdir plugin's pytest_sessionfinish) finds it there.
    request.config._tmp_path_factory = factory
    yield factory


def session_finish(config, exitstatus):
    """Upstream tmpdir plugin's pytest_sessionfinish, called by the engine
    when the session finishes (outside any test's warning capture; warnings
    are recorded with when="config"): a fully-passed run under the "failed"
    policy removes the whole basetemp (best effort, errors ignored), dead
    symlinks are removed, then the factory's exit stack (lock removal,
    numbered-dir pruning) is closed."""
    factory = getattr(config, "_tmp_path_factory", None)
    if factory is None:
        return
    config._tmp_path_factory = None
    basetemp = factory._basetemp
    if basetemp is None:
        return
    if (
        exitstatus == 0
        and factory._retention_policy == "failed"
        and factory._given_basetemp is None
    ):
        if basetemp.is_dir():
            shutil.rmtree(basetemp, ignore_errors=True)
    if basetemp.is_dir():
        from _pytest.pathlib import cleanup_dead_symlinks

        cleanup_dead_symlinks(basetemp)
    factory._exit_stack.close()


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
        # Best effort, like upstream: a dir the test left unremovable stays.
        shutil.rmtree(path, ignore_errors=True)


class _NeverRaised(Exception):
    """Sentinel so ``LocalPath.visit``'s default ``ignore`` never matches."""


class LocalPath:
    """Minimal py.path.local equivalent backing the legacy tmpdir fixture."""

    def __init__(self, path):
        self._path = pathlib.Path(os.path.realpath(path))

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

    def __lt__(self, other):
        return str(self) < str(other)

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

    def dirpath(self, *parts, abs=False):
        parent = self._path.parent
        if abs:
            return LocalPath(pathlib.PurePosixPath(*[str(p) for p in parts]))
        stripped = [str(p).lstrip("/\\") for p in parts]
        return LocalPath(parent.joinpath(*stripped))

    def ensure(self, *parts, **kwargs):
        # Upstream takes **kwargs and only looks at `dir`; callers also pass
        # e.g. `file=True` (the default).
        target = self._path.joinpath(*[str(part) for part in parts])
        if kwargs.get("dir", 0):
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

    def write_binary(self, data):
        self._path.write_bytes(data)

    def read_binary(self):
        return self._path.read_bytes()

    def read(self, mode="r"):
        with open(self._path, mode) as f:
            return f.read()

    def read_text(self, encoding="utf-8"):
        return self._path.read_text(encoding=encoding)

    def listdir(self, fil=None, sort=True):
        entries = sorted(self._path.iterdir())
        if fil is None:
            pass
        elif callable(fil):
            entries = [e for e in entries if fil(LocalPath(e))]
        else:
            entries = [e for e in entries if fnmatch.fnmatch(e.name, fil)]
        return [LocalPath(child) for child in entries]

    def computehash(self, hashtype="md5"):
        h = hashlib.new(hashtype)
        h.update(self._path.read_bytes())
        return h.hexdigest()

    def move(self, target):
        return LocalPath(shutil.move(str(self._path), str(target)))

    def chdir(self):
        old = os.getcwd()
        os.chdir(self._path)
        return LocalPath(old)

    def visit(self, fil=None, rec=None, ignore=_NeverRaised, bf=False, sort=False):
        """py.path.local.visit: yield paths below this one matching ``fil``,
        recursing into dirs matching ``rec`` (default: recurse into all)."""
        if isinstance(fil, str):
            fil = LocalPath._fnmatcher(fil)
        if isinstance(rec, str):
            rec = LocalPath._fnmatcher(rec)
        elif rec and not callable(rec):
            rec = LocalPath._always_recurse
        yield from self._visit_entries(fil, rec, ignore, bf)

    @staticmethod
    def _always_recurse(_path):
        return True

    def _visit_entries(self, fil, rec, ignore, bf):
        try:
            entries = self.listdir()
        except ignore:
            return
        dirs = [p for p in entries if p.isdir() and (rec is None or rec(p))]
        if not bf:
            for subdir in dirs:
                yield from subdir._visit_entries(fil, rec, ignore, bf)
        for p in entries:
            if fil is None or fil(p):
                yield p
        if bf:
            for subdir in dirs:
                yield from subdir._visit_entries(fil, rec, ignore, bf)

    @staticmethod
    def _fnmatcher(pattern):
        def match(path):
            sep = os.sep
            pat = pattern
            if pat.find(sep) == -1:
                name = path.basename
            else:
                name = str(path)
                if not os.path.isabs(pat):
                    pat = "*" + sep + pat
            return fnmatch.fnmatch(name, pat)

        return match


@fixture
def tmpdir(tmp_path):
    return LocalPath(tmp_path)


class TempdirFactory:
    """Backward-compat wrapper around TempPathFactory (py.path.local)."""

    def __init__(self, tmp_path_factory):
        self._tmp_path_factory = tmp_path_factory

    def mktemp(self, basename, numbered=True):
        return LocalPath(self._tmp_path_factory.mktemp(basename, numbered=numbered))

    def getbasetemp(self):
        return LocalPath(self._tmp_path_factory.getbasetemp())


@fixture(scope="session")
def tmpdir_factory(tmp_path_factory):
    return TempdirFactory(tmp_path_factory)
