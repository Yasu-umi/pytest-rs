"""Mocking: pytest-mock is bundled — no extra install needed."""

import my_project


def test_mock_return_value(mocker):
    mocker.patch.object(my_project, "add", return_value=42)
    assert my_project.add(1, 2) == 42


def test_mock_greet(mocker):
    mocker.patch.object(my_project, "greet", return_value="Hi!")
    assert my_project.greet("world") == "Hi!"


async def test_mock_async(mocker):
    mocker.patch.object(
        my_project,
        "fetch_json",
        return_value={"url": "https://mocked.test", "status": 404},
    )
    result = await my_project.fetch_json("https://example.com")
    assert result["status"] == 404
