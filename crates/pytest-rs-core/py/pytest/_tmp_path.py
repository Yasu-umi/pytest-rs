"""The tmp_path builtin fixture."""

from pytest._fixtures import fixture


@fixture
def tmp_path():
    import pathlib
    import shutil
    import tempfile

    path = pathlib.Path(tempfile.mkdtemp(prefix="pytest-rs-tmp-"))
    yield path
    shutil.rmtree(path, ignore_errors=True)
