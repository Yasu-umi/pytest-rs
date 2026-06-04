import os
import pathlib

from pytest import skip


def symlink_or_skip(src, dst, **kwargs):
    """Make a symlink, or skip the test in case symlinks are not supported."""
    try:
        os.symlink(str(src), str(dst), **kwargs)
    except OSError as e:
        skip(f"symlinks not supported: {e}")


def make_numbered_dir(root, prefix, mode=0o700):
    root = pathlib.Path(root)
    maximum = -1
    for path in root.iterdir():
        name = path.name
        if name.startswith(prefix) and name[len(prefix) :].isdigit():
            maximum = max(maximum, int(name[len(prefix) :]))
    new_path = root / f"{prefix}{maximum + 1}"
    new_path.mkdir(mode=mode)
    return new_path


def maybe_delete_a_numbered_dir(path):
    import shutil

    shutil.rmtree(path, ignore_errors=True)


def import_path(path, *, root=None, mode=None, consider_namespace_packages=False):
    import importlib.util

    path = pathlib.Path(path)
    spec = importlib.util.spec_from_file_location(path.stem, path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module
