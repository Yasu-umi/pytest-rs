"""The cache: `config.cache` / the `cache` fixture, backed by
{cache_dir}/v JSON files like pytest's cacheprovider."""

import errno
import json
import os
import shutil
import tempfile
import warnings
from datetime import datetime, timedelta
from pathlib import Path
from pprint import pformat

from pytest._fixtures import fixture
from pytest._warning_types import PytestCacheWarning

README_CONTENT = """\
# pytest cache directory #

This directory contains data from the pytest's cache plugin,
which provides the `--lf` and `--ff` options, as well as the `cache` fixture.

**Do not** commit this to version control.

See [the docs](https://docs.pytest.org/en/stable/how-to/cache.html) for more information.
"""

CACHEDIR_TAG_CONTENT = b"""\
Signature: 8a477f597d28d172789f06886806bc55
# This file is a cache directory tag created by pytest.
# For information about cache directory tags, see:
#\thttps://bford.info/cachedir/spec.html
"""

# The files a fresh cache dir is created with (pytest 9.2's CACHEDIR_FILES).
CACHEDIR_FILES = {
    "README.md": README_CONTENT.encode(),
    ".gitignore": b"# Created by pytest automatically.\n*\n",
    "CACHEDIR.TAG": CACHEDIR_TAG_CONTENT,
}


class Cache:
    # Sub-directory under cache-dir for directories created by `mkdir()`.
    _CACHE_PREFIX_DIRS = "d"

    # Sub-directory under cache-dir for values created by `set()`.
    _CACHE_PREFIX_VALUES = "v"

    def __init__(self, cachedir, _ispytest=False):
        self._cachedir = Path(cachedir)

    @staticmethod
    def default_cache_dir():
        """$TOX_ENV_DIR/.pytest_cache inside an active tox env, else
        .pytest_cache (pytest's cache_dir ini default)."""
        tox_env_dir = os.environ.get("TOX_ENV_DIR")
        if tox_env_dir:
            return os.path.join(tox_env_dir, ".pytest_cache")
        return ".pytest_cache"

    @classmethod
    def cache_dir_from(cls, rootdir, ini_cache_dir):
        """Resolve the `cache_dir` ini (default .pytest_cache) against the
        rootdir, with ~ and env vars expanded (pytest's resolve_from_str)."""
        value = os.path.expanduser(os.path.expandvars(ini_cache_dir or cls.default_cache_dir()))
        return str(Path(rootdir) / value)

    @classmethod
    def for_config(cls, config, *, _ispytest=False):
        return cls(cls.cache_dir_from(str(config.rootpath), config.getini("cache_dir")))

    def clear_cache(self):
        """--cache-clear: drop the value and directory stores."""
        for prefix in (self._CACHE_PREFIX_DIRS, self._CACHE_PREFIX_VALUES):
            d = self._cachedir / prefix
            if d.is_dir():
                shutil.rmtree(d, ignore_errors=True)

    def warn(self, fmt, *, _ispytest=False, **args):
        warnings.warn(PytestCacheWarning(fmt.format(**args) if args else fmt), stacklevel=3)

    def _mkdir(self, path):
        self._ensure_cache_dir_and_supporting_files()
        path.mkdir(exist_ok=True, parents=True)

    @staticmethod
    def _join_within(base, name):
        """Join ``name`` onto ``base``, keeping the result inside ``base``:
        an absolute ``name`` or ``..`` segments escaping it raise ValueError
        (pytest 9.2), while a ``..`` that cancels out within ``base`` is
        fine."""
        path = Path(os.path.normpath(base.joinpath(name)))
        if not path.is_relative_to(base):
            raise ValueError(f"{name!r} is not allowed to escape the cache directory")
        return path

    def mkdir(self, name):
        if len(Path(name).parts) > 1:
            raise ValueError("name is not allowed to contain path separators")
        res = self._join_within(self._cachedir / self._CACHE_PREFIX_DIRS, name)
        self._mkdir(res)
        return res

    def _getvaluepath(self, key):
        return self._join_within(self._cachedir / self._CACHE_PREFIX_VALUES, key)

    def get(self, key, default):
        path = self._getvaluepath(key)
        try:
            with path.open("r", encoding="UTF-8") as f:
                return json.load(f)
        except (ValueError, OSError):
            return default

    def set(self, key, value):
        path = self._getvaluepath(key)
        try:
            self._mkdir(path.parent)
        except OSError as exc:
            self.warn(f"could not create cache path {path}: {exc}")
            return
        data = json.dumps(value, ensure_ascii=False, indent=2)
        try:
            # A failed write or close warns too, not only a failed open.
            path.write_text(data, encoding="UTF-8")
        except OSError as exc:
            self.warn(f"cache could not write path {path}: {exc}")

    def _ensure_cache_dir_and_supporting_files(self):
        if not self._cachedir.is_dir():
            Cache._make_cachedir(self._cachedir)

    @staticmethod
    def _make_cachedir(target):
        """Create the cache dir with its supporting files atomically: fill a
        temporary sibling, then rename it into place. Losing a concurrent
        race just discards the temporary dir; the temporary dir never
        outlives the call, whatever is raised (pytest 9.2)."""
        target.parent.mkdir(parents=True, exist_ok=True)
        path = Path(tempfile.mkdtemp(prefix="pytest-cache-files-", dir=target.parent))
        try:
            # Reset permissions to the default, see pytest#12308.
            umask = os.umask(0o022)
            os.umask(umask)
            path.chmod(0o777 - umask)
            for name, content in CACHEDIR_FILES.items():
                path.joinpath(name).write_bytes(content)
            path.rename(target)
        except OSError as e:
            # Lost a concurrent-creation race: the cache dir now exists with
            # the same supporting files, so nothing left to do.
            if e.errno not in (errno.ENOTEMPTY, errno.EEXIST):
                raise
        finally:
            shutil.rmtree(path, ignore_errors=True)


def _sep(title):
    line = f" {title} "
    total = max(80 - len(line), 2)
    left = total // 2
    return "-" * left + line + "-" * (total - left)


def cacheshow(cachedir, glob):
    """--cache-show: list cached values and directories (pytest's
    cacheshow session)."""
    cache = Cache(cachedir)
    print("cachedir: " + str(cache._cachedir))
    if not cache._cachedir.is_dir():
        print("cache is empty")
        return 0

    dummy = object()
    basedir = cache._cachedir
    vdir = basedir / Cache._CACHE_PREFIX_VALUES
    print(_sep(f"cache values for {glob!r}"))

    def globfiles(base):
        # A glob may contain `..` segments, which rglob() follows: keep only
        # files that stay under `base` (pytest 9.2).
        for x in base.rglob(glob):
            if x.is_file() and Path(os.path.normpath(x)).is_relative_to(base):
                yield x

    for valpath in sorted(globfiles(vdir)):
        key = str(valpath.relative_to(vdir))
        val = cache.get(key, dummy)
        if val is dummy:
            print(f"{key} contains unreadable content, will be ignored")
        else:
            print(f"{key} contains:")
            for line in pformat(val).splitlines():
                print("  " + line)

    ddir = basedir / Cache._CACHE_PREFIX_DIRS
    if ddir.is_dir():
        print(_sep(f"cache directories for {glob!r}"))
        for p in sorted(globfiles(ddir)):
            key = str(p.relative_to(basedir))
            print(f"{key} is a file of length {p.stat().st_size}")
    return 0


def stepwise_info(cache_obj):
    """Return (last_failed, test_count, age_str, error_msg) from the stepwise cache entry.

    error_msg is non-None when the cache exists but is invalid (e.g. corrupted);
    in that case last_failed/test_count/age_str are all None.
    """
    info = cache_obj.get("cache/stepwise", None)
    if info is None:
        return None, None, None, None
    if isinstance(info, str):
        # Legacy format: bare nodeid string.
        return info, None, None, None
    if isinstance(info, dict):
        try:
            last_failed = info["last_failed"]
            test_count = info["last_test_count"]
            cache_date_str = info["last_cache_date_str"]
        except (KeyError, TypeError) as e:
            error = f"{type(e).__name__}: {e}"
            return None, None, None, f"error reading cache, discarding ({error})"
        age_str = None
        if cache_date_str:
            try:
                cache_date = datetime.fromisoformat(cache_date_str)
                age = datetime.now() - cache_date
                age = timedelta(seconds=int(age.total_seconds()))
                age_str = str(age)
            except (ValueError, TypeError):
                pass
        return last_failed, test_count, age_str, None
    return None, None, None, None


def stepwise_write(cache_obj, nodeid, test_count=None):
    """Persist the stepwise resume point (or clear it with nodeid=None)."""
    if nodeid is None:
        cache_obj.set("cache/stepwise", None)
    else:
        cache_obj.set(
            "cache/stepwise",
            {
                "last_failed": nodeid,
                "last_test_count": test_count,
                "last_cache_date_str": datetime.now().isoformat(),
            },
        )


@fixture
def cache(request):
    return request.config.cache
