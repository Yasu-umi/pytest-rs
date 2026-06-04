"""unittest.TestCase integration: build zero-arg runners per test method."""

import unittest

from pytest._outcomes import Skipped


def is_testcase_class(obj):
    try:
        return isinstance(obj, type) and issubclass(obj, unittest.TestCase)
    except TypeError:
        return False


def make_runner(cls, method_name):
    """A zero-arg callable running setUp/method/tearDown with SkipTest
    mapped onto pytest's Skipped."""

    def run():
        __tracebackhide__ = True
        case = cls(method_name)
        # Class-level skip decorators.
        if getattr(cls, "__unittest_skip__", False):
            raise Skipped(msg=getattr(cls, "__unittest_skip_why__", ""))
        method = getattr(case, method_name)
        if getattr(method, "__unittest_skip__", False):
            raise Skipped(msg=getattr(method, "__unittest_skip_why__", ""))
        try:
            case.setUp()
        except unittest.SkipTest as e:
            raise Skipped(msg=str(e)) from None
        try:
            try:
                method()
            except unittest.SkipTest as e:
                raise Skipped(msg=str(e)) from None
        finally:
            case.tearDown()

    return run


def class_setup(cls):
    setup = getattr(cls, "setUpClass", None)
    if setup is not None:
        try:
            setup()
        except unittest.SkipTest as e:
            raise Skipped(msg=str(e)) from None


def class_teardown(cls):
    teardown = getattr(cls, "tearDownClass", None)
    if teardown is not None:
        teardown()
