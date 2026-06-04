"""_pytest internals shim: upstream test suites import these names.

Only the surface needed by upstream suites is provided; most of it
re-exports the pytest shim's objects.
"""

from pytest import __version__ as __version__  # noqa: F401
from pytest import version_tuple as version_tuple  # noqa: F401
