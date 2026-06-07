"""Functional demos for autoloaded fixture-provider plugins: each test
exercises the plugin's actual fixture, so a silently-broken autoload fails
the smoke run instead of passing vacuously."""

import datetime

import requests


def test_faker(faker):
    name = faker.name()
    assert isinstance(name, str)
    assert name


def test_time_machine(time_machine):
    time_machine.move_to(datetime.datetime(2005, 4, 2, tzinfo=datetime.UTC))
    assert datetime.datetime.now(tz=datetime.UTC).year == 2005


def test_requests_mock(requests_mock):
    requests_mock.get("https://example.test/ping", json={"ok": True})
    assert requests.get("https://example.test/ping").json() == {"ok": True}
