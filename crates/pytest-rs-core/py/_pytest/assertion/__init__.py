"""Assertion-introspection surface: the builtin pytest_assertrepr_compare
(upstream pytest 9.2's, over the vendored util/truncate modules)."""

from _pytest.assertion import truncate as truncate
from _pytest.assertion import util as util
from _pytest.assertion._typing import NO_TRUNCATION_BUDGET, TruncationBudget


def pytest_assertrepr_compare(config, op, left, right):
    if config.pluginmanager.has_plugin("terminalreporter"):
        highlighter = config.get_terminal_writer()._highlight
    else:
        # Keep it plaintext when not using terminalreporter (#14377).
        highlighter = util.dummy_highlighter
    # When truncation is going to clip the explanation downstream, cap the
    # comparison helpers' formatting at what the truncator will actually pull.
    should_truncate, base_budget = truncate._get_truncation_parameters(config)
    if should_truncate:
        truncation_budget = TruncationBudget(
            max_lines=base_budget.max_lines + truncate.TRUNCATION_FOOTER_LINES + 1
            if base_budget.max_lines > 0
            else 0,
            max_chars=base_budget.max_chars + truncate.TRUNCATION_FOOTER_CHARS
            if base_budget.max_chars > 0
            else 0,
        )
    else:
        truncation_budget = NO_TRUNCATION_BUDGET
    lines = util.assertrepr_compare(
        op=op,
        left=left,
        right=right,
        verbose=config.get_verbosity("assertions"),
        highlighter=highlighter,
        assertion_text_diff_style=config.getini("assertion_text_diff_style"),
        truncation_budget=truncation_budget,
    )
    return truncate.materialize_with_truncation(lines, config) or None


from _pytest._stub import __getattr__  # noqa: E402, F401
