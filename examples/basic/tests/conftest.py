import pytest


@pytest.fixture
def sample_list():
    return [3, 1, 4, 1, 5, 9, 2, 6]


@pytest.fixture
def config_file(tmp_path):
    path = tmp_path / "config.toml"
    path.write_text('[project]\nname = "demo"\n')
    return path
