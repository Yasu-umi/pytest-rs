"""The cache: `config.cache` / the `cache` fixture, backed by
{cache_dir}/v JSON files like pytest's cacheprovider."""

import json
import os
from pathlib import Path

from pytest._fixtures import fixture

README_CONTENT = """\
# pytest cache directory #

This directory contains data from the pytest's cache plugin,
which provides the `--lf` and `--ff` options, as well as the `cache` fixture.

**Do not** commit this to version control.

See [the docs](https://docs.pytest.org/en/stable/how-to/cache.html) for more information.
"""

CACHEDIR_TAG_CONTENT = """\
Signature: 8a477f597d28d172789f06886806bc55
# This file is a cache directory tag created by pytest.
# For information about cache directory tags, see:
#\thttps://bford.info/cachedir/spec.html
"""


class Cache:
    # Sub-directory under cache-dir for directories created by `mkdir()`.
    _CACHE_PREFIX_DIRS = "d"

    # Sub-directory under cache-dir for values created by `set()`.
    _CACHE_PREFIX_VALUES = "v"

    def __init__(self, cachedir):
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
    def for_config(cls, config):
        return cls(cls.cache_dir_from(str(config.rootpath), config.getini("cache_dir")))

    def clear_cache(self):
        """--cache-clear: drop the value and directory stores."""
        import shutil

        for prefix in (self._CACHE_PREFIX_DIRS, self._CACHE_PREFIX_VALUES):
            d = self._cachedir / prefix
            if d.is_dir():
                shutil.rmtree(d, ignore_errors=True)

    def warn(self, fmt, **args):
        import warnings

        from pytest._warning_types import PytestCacheWarning

        warnings.warn(PytestCacheWarning(fmt.format(**args) if args else fmt), stacklevel=3)

    def _mkdir(self, path):
        self._ensure_cache_dir_and_supporting_files()
        path.mkdir(exist_ok=True, parents=True)

    def mkdir(self, name):
        path = Path(name)
        if len(path.parts) > 1:
            raise ValueError("name is not allowed to contain path separators")
        res = self._cachedir.joinpath(self._CACHE_PREFIX_DIRS, path)
        self._mkdir(res)
        return res

    def _getvaluepath(self, key):
        return self._cachedir.joinpath(self._CACHE_PREFIX_VALUES, Path(key))

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
            f = path.open("w", encoding="UTF-8")
        except OSError as exc:
            self.warn(f"cache could not write path {path}: {exc}")
        else:
            with f:
                f.write(data)

    def _ensure_cache_dir_and_supporting_files(self):
        if self._cachedir.is_dir():
            return
        self._cachedir.parent.mkdir(parents=True, exist_ok=True)
        import tempfile

        with tempfile.TemporaryDirectory(
            prefix="pytest-cache-files-", dir=self._cachedir.parent
        ) as newpath:
            path = Path(newpath)
            # Reset permissions to the default, see pytest#12308.
            umask = os.umask(0o022)
            os.umask(umask)
            path.chmod(0o777 - umask)
            with open(path.joinpath("README.md"), "x", encoding="UTF-8") as f:
                f.write(README_CONTENT)
            with open(path.joinpath(".gitignore"), "x", encoding="UTF-8") as f:
                f.write("# Created by pytest automatically.\n*\n")
            with open(path.joinpath("CACHEDIR.TAG"), "xb") as f:
                f.write(CACHEDIR_TAG_CONTENT.encode())
            try:
                path.rename(self._cachedir)
            except OSError as e:
                import errno

                # Lost a concurrent-creation race: the cache dir now exists
                # with the same supporting files, so nothing left to do.
                if e.errno not in (errno.ENOTEMPTY, errno.EEXIST):
                    raise
            else:
                # Recreate so TemporaryDirectory's cleanup finds its dir.
                path.mkdir()


def _sep(title):
    line = f" {title} "
    total = max(80 - len(line), 2)
    left = total // 2
    return "-" * left + line + "-" * (total - left)


def cacheshow(cachedir, glob):
    """--cache-show: list cached values and directories (pytest's
    cacheshow session)."""
    from pprint import pformat

    cache = Cache(cachedir)
    print("cachedir: " + str(cache._cachedir))
    if not cache._cachedir.is_dir():
        print("cache is empty")
        return 0

    dummy = object()
    basedir = cache._cachedir
    vdir = basedir / Cache._CACHE_PREFIX_VALUES
    print(_sep(f"cache values for {glob!r}"))
    for valpath in sorted(x for x in vdir.rglob(glob) if x.is_file()):
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
        contents = sorted(ddir.rglob(glob))
        print(_sep(f"cache directories for {glob!r}"))
        for p in contents:
            if p.is_file():
                key = str(p.relative_to(basedir))
                print(f"{key} is a file of length {p.stat().st_size}")
    return 0


@fixture
def cache(request):
    return request.config.cache
