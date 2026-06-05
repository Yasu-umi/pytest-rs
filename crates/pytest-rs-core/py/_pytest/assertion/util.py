"""Selected real implementations of _pytest.assertion.util (ported from
pytest, simplified); everything else falls back to the raising stub."""

import os
from collections.abc import Sequence

from _pytest._stub import __getattr__  # noqa: E402, F401


def running_on_ci():
    return any(var in os.environ for var in ["CI", "BUILD_NUMBER"])


def istext(value):
    return isinstance(value, str)


def isdict(value):
    return isinstance(value, dict)


def isset(value):
    return isinstance(value, (set, frozenset))


def issequence(value):
    return isinstance(value, Sequence) and not istext(value)


def isiterable(obj):
    try:
        iter(obj)
        return not istext(obj)
    except Exception:
        return False


def _compare_eq_any(left, right, highlighter, verbose=0):
    explanation = []
    if istext(left) and istext(right):
        return explanation
    if issequence(left) and issequence(right):
        explanation = _compare_eq_sequence(left, right, highlighter, verbose)
    elif isset(left) and isset(right):
        explanation = _compare_eq_set(left, right, highlighter, verbose)
    elif isdict(left) and isdict(right):
        explanation = _compare_eq_dict(left, right, highlighter, verbose)
    if isiterable(left) and isiterable(right):
        explanation.extend(_compare_eq_iterable(left, right, highlighter, verbose))
    return explanation


def _compare_eq_iterable(left, right, highlighter, verbose=0):
    if verbose <= 0 and not running_on_ci():
        return ["Use -v to get more diff"]
    import difflib
    import pprint

    left_formatting = pprint.pformat(left).splitlines()
    right_formatting = pprint.pformat(right).splitlines()
    explanation = ["", "Full diff:"]
    explanation.extend(
        highlighter(
            "\n".join(line.rstrip() for line in difflib.ndiff(right_formatting, left_formatting)),
            lexer="diff",
        ).splitlines()
    )
    return explanation


def _compare_eq_sequence(left, right, highlighter, verbose=0):
    explanation = []
    len_left = len(left)
    len_right = len(right)
    for i in range(min(len_left, len_right)):
        if left[i] != right[i]:
            explanation.append(f"At index {i} diff: {left[i]!r} != {right[i]!r}")
            break

    if len_left > len_right:
        if len_left - len_right == 1:
            explanation.append(f"Left contains one more item: {left[len_right]!r}")
        else:
            explanation.append(
                f"Left contains {len_left - len_right} more items, "
                f"first extra item: {left[len_right]!r}"
            )
    elif len_left < len_right:
        if len_right - len_left == 1:
            explanation.append(f"Right contains one more item: {right[len_left]!r}")
        else:
            explanation.append(
                f"Right contains {len_right - len_left} more items, "
                f"first extra item: {right[len_left]!r}"
            )
    return explanation


def _set_one_sided_diff(posn, set1, set2):
    explanation = []
    diff = set1 - set2
    if diff:
        explanation.append(f"Extra items in the {posn} set:")
        explanation.extend(repr(item) for item in diff)
    return explanation


def _compare_eq_set(left, right, highlighter, verbose=0):
    explanation = []
    explanation.extend(_set_one_sided_diff("left", left, right))
    explanation.extend(_set_one_sided_diff("right", right, left))
    return explanation


def _compare_eq_dict(left, right, highlighter, verbose=0):
    import pprint

    explanation = []
    set_left = set(left)
    set_right = set(right)
    common = set_left.intersection(set_right)
    same = {k: left[k] for k in common if left[k] == right[k]}
    if same and verbose < 2:
        explanation += [f"Omitting {len(same)} identical items, use -vv to show"]
    elif same:
        explanation += ["Common items:"]
        explanation += pprint.pformat(same).splitlines()
    diff = {k for k in common if left[k] != right[k]}
    if diff:
        explanation += ["Differing items:"]
        for k in diff:
            explanation += [repr({k: left[k]}) + " != " + repr({k: right[k]})]
    extra_left = set_left - set_right
    len_extra_left = len(extra_left)
    if len_extra_left:
        explanation.append(
            f"Left contains {len_extra_left} more item{'' if len_extra_left == 1 else 's'}:"
        )
        explanation.extend(pprint.pformat({k: left[k] for k in extra_left}).splitlines())
    extra_right = set_right - set_left
    len_extra_right = len(extra_right)
    if len_extra_right:
        explanation.append(
            f"Right contains {len_extra_right} more item{'' if len_extra_right == 1 else 's'}:"
        )
        explanation.extend(pprint.pformat({k: right[k] for k in extra_right}).splitlines())
    return explanation
