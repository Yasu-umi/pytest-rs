"""`_pytest.approx` (pytest 9.2 moved approx here from `_pytest.python_api`).

Re-exports pytest-rs's approx implementation (`pytest._approx`) under the
names pytest 9.2's module defines, so code importing them from here — the
upstream test suite, plugins — gets the classes `pytest.approx` builds.
"""

from __future__ import annotations

from pytest._approx import ApproxBase as ApproxBase
from pytest._approx import ApproxDecimal as ApproxDecimal
from pytest._approx import ApproxMapping as ApproxMapping
from pytest._approx import ApproxNumpy as ApproxNumpy
from pytest._approx import ApproxScalar as ApproxScalar
from pytest._approx import ApproxSequenceLike as ApproxSequenceLike
from pytest._approx import approx as approx

from _pytest.python_api import _recursive_sequence_map as _recursive_sequence_map

# pytest 9.2 renamed the base class ApproxBase -> Approx.
Approx = ApproxBase

__all__ = [
    "Approx",
    "ApproxBase",
    "ApproxDecimal",
    "ApproxMapping",
    "ApproxNumpy",
    "ApproxScalar",
    "ApproxSequenceLike",
    "approx",
]
