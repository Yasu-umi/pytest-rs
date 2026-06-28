"""Helper plugin for pytester; should not be loaded on its own.

This module is kept separate from pytester.py so that assertion rewriting
can apply to its assert statements (pytester.py itself is imported too early
to be rewritten).
"""
# NOTE: keep this import at top so rewriter sees it before other imports
from __future__ import annotations


def assertoutcome(
    outcomes,
    passed: int = 0,
    skipped: int = 0,
    failed: int = 0,
) -> None:
    __tracebackhide__ = True

    realpassed, realskipped, realfailed = outcomes
    obtained = {
        "passed": len(realpassed),
        "skipped": len(realskipped),
        "failed": len(realfailed),
    }
    expected = {"passed": passed, "skipped": skipped, "failed": failed}
    assert obtained == expected, outcomes


def assert_outcomes(
    outcomes: dict,
    passed: int = 0,
    skipped: int = 0,
    failed: int = 0,
    errors: int = 0,
    xpassed: int = 0,
    xfailed: int = 0,
    warnings: int | None = None,
    deselected: int | None = None,
) -> None:
    """Assert that the specified outcomes appear with the respective
    numbers (0 means it didn't occur) in the text output from a test run."""
    __tracebackhide__ = True

    obtained = {
        "passed": outcomes.get("passed", 0),
        "skipped": outcomes.get("skipped", 0),
        "failed": outcomes.get("failed", 0),
        "errors": outcomes.get("errors", 0),
        "xpassed": outcomes.get("xpassed", 0),
        "xfailed": outcomes.get("xfailed", 0),
    }
    expected = {
        "passed": passed,
        "skipped": skipped,
        "failed": failed,
        "errors": errors,
        "xpassed": xpassed,
        "xfailed": xfailed,
    }
    if warnings is not None:
        obtained["warnings"] = outcomes.get("warnings", 0)
        expected["warnings"] = warnings
    if deselected is not None:
        obtained["deselected"] = outcomes.get("deselected", 0)
        expected["deselected"] = deselected
    assert obtained == expected
