"""pytest.approx: pytest 9.2's implementation, shipped as ``_pytest.approx``
(ported from upstream as-is). Re-exported here under the names the rest of
the shim imports."""

from _pytest.approx import Approx as Approx
from _pytest.approx import ApproxDecimal as ApproxDecimal
from _pytest.approx import ApproxMapping as ApproxMapping
from _pytest.approx import ApproxNumpy as ApproxNumpy
from _pytest.approx import ApproxScalar as ApproxScalar
from _pytest.approx import ApproxSequenceLike as ApproxSequenceLike
from _pytest.approx import _recursive_sequence_map as _recursive_sequence_map
from _pytest.approx import approx as approx

# Pre-9.2 names of the base class.
ApproxBase = Approx
_Approx = Approx
