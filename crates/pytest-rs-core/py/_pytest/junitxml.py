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
from _pytest.stash import StashKey

xml_key = StashKey["LogXML"]()


def pytest_configure(config):
    xmlpath = config.option.xmlpath
    if xmlpath and not hasattr(config, "workerinput"):
        config.stash[xml_key] = LogXML(
            xmlpath,
            config.option.junitprefix,
            config.getini("junit_suite_name"),
            config.getini("junit_logging"),
            config.getini("junit_duration_report"),
            config.getini("junit_family"),
            config.getini("junit_log_passing_tests"),
        )
        config.pluginmanager.register(config.stash[xml_key])


def pytest_unconfigure(config):
    xml = config.stash.get(xml_key, None)
    if xml:
        del config.stash[xml_key]
        config.pluginmanager.unregister(xml)
