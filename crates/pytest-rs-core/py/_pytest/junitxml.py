from pytest._junitxml import LogXML as LogXML
from pytest._junitxml import _NodeReporter as _NodeReporter
from pytest._junitxml import bin_xml_escape as bin_xml_escape
from pytest._junitxml import families as families
from pytest._junitxml import mangle_test_address as mangle_test_address
from pytest._junitxml import merge_family as merge_family
from pytest._junitxml import record_property as record_property
from pytest._junitxml import record_testsuite_property as record_testsuite_property
from pytest._junitxml import record_xml_attribute as record_xml_attribute

from _pytest._stub import __getattr__  # noqa: E402, F401
