"""Path helpers ported from _pytest/pathlib.py (the numbered-dir cleanup
machinery is upstream's, byte-for-byte where practical: upstream's own
test suite drives these functions directly)."""

import atexit
import contextlib
import importlib.util
import itertools
import os
import pathlib
import shutil
import stat
import sys
import uuid
import warnings
from errno import EBADF, ELOOP, ENOENT, ENOTDIR
from functools import partial
from pathlib import Path

from pytest import skip

from _pytest.warning_types import PytestWarning


def absolutepath(path):
    """Convert a path to an absolute path, without resolving symlinks
    (pytest's absolutepath)."""
    return Path(os.path.abspath(path))


def safe_exists(p):
    """exists() that swallows the ValueError Windows raises for over-long
    paths (pytest's safe_exists)."""
    try:
        return Path(p).exists()
    except (ValueError, OSError):
        return False


# The following constants are taken from CPython's shutil (upstream).
_IGNORED_ERRORS = (ENOENT, ENOTDIR, EBADF, ELOOP)
_IGNORED_WINERRORS = (
    21,  # ERROR_NOT_READY - drive exists but is not accessible
    1921,  # ERROR_CANT_RESOLVE_FILENAME - fix for broken symlink pointing to itself
)


def _ignore_error(exception):
    return (
        getattr(exception, "errno", None) in _IGNORED_ERRORS
        or getattr(exception, "winerror", None) in _IGNORED_WINERRORS
    )


def get_lock_path(path):
    return path.joinpath(".lock")


def on_rm_rf_error(func, path, excinfo, *, start_path):
    """Handle known read-only errors during rmtree.

    The returned value is used only by our own tests.
    """
    if isinstance(excinfo, BaseException):
        exc = excinfo
    else:
        exc = excinfo[1]

    # Another process removed the file in the middle of the "rm_rf" (xdist
    # for example). More context:
    # https://github.com/pytest-dev/pytest/issues/5974#issuecomment-543799018
    if isinstance(exc, FileNotFoundError):
        return False

    if not isinstance(exc, PermissionError):
        warnings.warn(PytestWarning(f"(rm_rf) error removing {path}\n{type(exc)}: {exc}"))
        return False

    if func not in (os.rmdir, os.remove, os.unlink):
        if func not in (os.open,):
            warnings.warn(
                PytestWarning(
                    f"(rm_rf) unknown function {func} when removing {path}:\n{type(exc)}: {exc}"
                )
            )
        return False

    # Chmod + retry.
    def chmod_rw(p):
        mode = os.stat(p).st_mode
        os.chmod(p, mode | stat.S_IRUSR | stat.S_IWUSR)

    # For files, we need to recursively go upwards in the directories to
    # ensure they all are also writable.
    p = Path(path)
    if p.is_file():
        for parent in p.parents:
            chmod_rw(str(parent))
            # Stop when we reach the original path passed to rm_rf.
            if parent == start_path:
                break
    chmod_rw(str(path))

    func(path)
    return True


def ensure_extended_length_path(path):
    """Get the extended-length version of a path (Windows); unchanged on
    other platforms."""
    if sys.platform.startswith("win32"):
        path = path.resolve()
        path = Path(get_extended_length_path_str(str(path)))
    return path


def get_extended_length_path_str(path):
    """Convert a path to a Windows extended length path."""
    long_path_prefix = "\\\\?\\"
    unc_long_path_prefix = "\\\\?\\UNC\\"
    if path.startswith((long_path_prefix, unc_long_path_prefix)):
        return path
    # UNC
    if path.startswith("\\\\"):
        return unc_long_path_prefix + path[2:]
    return long_path_prefix + path


def rm_rf(path):
    """Remove the path contents recursively, even if some elements
    are read-only."""
    path = ensure_extended_length_path(pathlib.Path(path))
    onerror = partial(on_rm_rf_error, start_path=path)
    shutil.rmtree(str(path), onexc=onerror)


def find_prefixed(root, prefix):
    """Find all elements in root that begin with the prefix, case-insensitive."""
    l_prefix = prefix.lower()
    for x in os.scandir(root):
        if x.name.lower().startswith(l_prefix):
            yield x


def extract_suffixes(iter, prefix):
    """Return the parts of the paths following the prefix."""
    p_len = len(prefix)
    for entry in iter:
        yield entry.name[p_len:]


def find_suffixes(root, prefix):
    """Combine find_prefixes and extract_suffixes."""
    return extract_suffixes(find_prefixed(root, prefix), prefix)


def parse_num(maybe_num):
    """Parse number path suffixes, returns -1 on error."""
    try:
        return int(maybe_num)
    except ValueError:
        return -1


def _force_symlink(root, target, link_to):
    """Helper to create the current symlink.

    It's full of race conditions that are reasonably OK to ignore
    for the context of best effort linking to the latest test run.
    """
    current_symlink = root.joinpath(target)
    try:
        current_symlink.unlink()
    except OSError:
        pass
    try:
        current_symlink.symlink_to(link_to)
    except Exception:
        pass


def make_numbered_dir(root, prefix, mode=0o700):
    """Create a directory with an increased number as suffix for the given prefix."""
    root = pathlib.Path(root)
    for _i in range(10):
        # try up to 10 times to create the folder
        max_existing = max(map(parse_num, find_suffixes(root, prefix)), default=-1)
        new_number = max_existing + 1
        new_path = root.joinpath(f"{prefix}{new_number}")
        try:
            new_path.mkdir(mode=mode)
        except Exception:
            pass
        else:
            _force_symlink(root, prefix + "current", new_path)
            return new_path
    raise OSError(f"could not create numbered dir with prefix {prefix} in {root} after 10 tries")


def create_cleanup_lock(p):
    """Create a lock to prevent premature folder cleanup."""
    lock_path = get_lock_path(p)
    try:
        fd = os.open(str(lock_path), os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o644)
    except FileExistsError as e:
        raise OSError(f"cannot create lockfile in {p}") from e
    else:
        pid = os.getpid()
        spid = str(pid).encode()
        os.write(fd, spid)
        os.close(fd)
        if not lock_path.is_file():
            raise OSError("lock path got renamed after successful creation")
        return lock_path


def register_cleanup_lock_removal(lock_path, register=atexit.register):
    """Register a cleanup function for removing a lock, by default on atexit."""
    pid = os.getpid()

    def cleanup_on_exit(lock_path=lock_path, original_pid=pid):
        current_pid = os.getpid()
        if current_pid != original_pid:
            # fork
            return
        try:
            lock_path.unlink()
        except OSError:
            pass

    return register(cleanup_on_exit)


def maybe_delete_a_numbered_dir(path):
    """Remove a numbered directory if its lock can be obtained and it does
    not seem to be in use."""
    path = ensure_extended_length_path(pathlib.Path(path))
    lock_path = None
    try:
        lock_path = create_cleanup_lock(path)
        parent = path.parent

        garbage = parent.joinpath(f"garbage-{uuid.uuid4()}")
        path.rename(garbage)
        rm_rf(garbage)
    except OSError:
        #  known races:
        #  * other process did a cleanup at the same time
        #  * deletable folder was found
        #  * process cwd (Windows)
        return
    finally:
        # If we created the lock, ensure we remove it even if we failed
        # to properly remove the numbered dir.
        if lock_path is not None:
            try:
                lock_path.unlink()
            except OSError:
                pass


def ensure_deletable(path, consider_lock_dead_if_created_before):
    """Check if `path` is deletable based on whether the lock file is expired."""
    if path.is_symlink():
        return False
    lock = get_lock_path(path)
    try:
        if not lock.is_file():
            return True
    except OSError:
        # we might not have access to the lock file at all, in this case
        # assume we don't have access to the entire directory (#7491).
        return False
    try:
        lock_time = lock.stat().st_mtime
    except Exception:
        return False
    else:
        if lock_time < consider_lock_dead_if_created_before:
            # We want to ignore any errors while trying to remove the lock.
            with contextlib.suppress(OSError):
                lock.unlink()
                return True
        return False


def try_cleanup(path, consider_lock_dead_if_created_before):
    """Try to cleanup a folder if we can ensure it's deletable."""
    if ensure_deletable(path, consider_lock_dead_if_created_before):
        maybe_delete_a_numbered_dir(path)


def cleanup_candidates(root, prefix, keep):
    """List candidates for numbered directories to be removed - follows py.path."""
    max_existing = max(map(parse_num, find_suffixes(root, prefix)), default=-1)
    max_delete = max_existing - keep
    entries = find_prefixed(root, prefix)
    entries, entries2 = itertools.tee(entries)
    numbers = map(parse_num, extract_suffixes(entries2, prefix))
    for entry, number in zip(entries, numbers, strict=True):
        if number <= max_delete:
            yield Path(entry)


def cleanup_dead_symlinks(root):
    for left_dir in root.iterdir():
        if left_dir.is_symlink():
            if not left_dir.resolve().exists():
                left_dir.unlink()


def cleanup_numbered_dir(root, prefix, keep, consider_lock_dead_if_created_before):
    """Cleanup for lock driven numbered directories."""
    if not root.exists():
        return
    for path in cleanup_candidates(root, prefix, keep):
        try_cleanup(path, consider_lock_dead_if_created_before)
    for path in root.glob("garbage-*"):
        try_cleanup(path, consider_lock_dead_if_created_before)

    cleanup_dead_symlinks(root)


def make_numbered_dir_with_cleanup(root, prefix, keep, lock_timeout, mode):
    """Create a numbered dir with a cleanup lock and remove old ones."""
    e = None
    for _i in range(10):
        try:
            p = make_numbered_dir(root, prefix, mode)
            # Only lock the current dir when keep is not 0
            if keep != 0:
                lock_path = create_cleanup_lock(p)
                register_cleanup_lock_removal(lock_path)
        except Exception as exc:
            e = exc
        else:
            consider_lock_dead_if_created_before = p.stat().st_mtime - lock_timeout
            # Register a cleanup for program exit
            atexit.register(
                cleanup_numbered_dir,
                root,
                prefix,
                keep,
                consider_lock_dead_if_created_before,
            )
            return p
    assert e is not None
    raise e


def symlink_or_skip(src, dst, **kwargs):
    """Make a symlink, or skip the test in case symlinks are not supported."""
    try:
        os.symlink(str(src), str(dst), **kwargs)
    except OSError as e:
        skip(f"symlinks not supported: {e}")


def import_path(path, *, root=None, mode=None, consider_namespace_packages=False):
    path = pathlib.Path(path)
    module_name = path.stem
    spec = importlib.util.spec_from_file_location(module_name, path)
    module = importlib.util.module_from_spec(spec)
    sys.modules[module_name] = module
    spec.loader.exec_module(module)
    return module


from _pytest._stub import __getattr__  # noqa: E402, F401
