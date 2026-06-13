"""Capture fixtures: capsys, caplog, monkeypatch, tmp_path."""

import logging
import os

import pytest

from my_project import greet


def test_capsys(capsys):
    print(greet("world"))
    captured = capsys.readouterr()
    assert "Hello, world!" in captured.out


def test_caplog(caplog):
    logger = logging.getLogger("demo")
    with caplog.at_level(logging.WARNING):
        logger.warning("disk almost full")
    assert "disk almost full" in caplog.text


def test_monkeypatch(monkeypatch):
    monkeypatch.setenv("APP_ENV", "testing")
    assert os.environ["APP_ENV"] == "testing"


def test_tmp_path(tmp_path):
    db = tmp_path / "data.db"
    db.write_bytes(b"\x00" * 64)
    assert db.stat().st_size == 64


@pytest.mark.slow
def test_slow_computation():
    total = sum(range(1_000_000))
    assert total == 499_999_500_000
