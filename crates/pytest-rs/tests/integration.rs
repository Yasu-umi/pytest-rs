//! End-to-end tests: build a small pytest suite in a temp dir, run the
//! pytest-rs binary against it, and assert on output + exit code.

use std::path::{Path, PathBuf};
use std::process::Output;

struct TempSuite {
    root: PathBuf,
}

impl TempSuite {
    fn new(name: &str) -> Self {
        let root = std::env::temp_dir()
            .join("pytest-rs-it")
            .join(format!("{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        Self { root }
    }

    fn write(&self, rel: &str, content: &str) -> &Self {
        let path = self.root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
        self
    }

    fn run(&self, args: &[&str]) -> Output {
        std::process::Command::new(env!("CARGO_BIN_EXE_pytest-rs-bin"))
            .args(args)
            .current_dir(&self.root)
            .output()
            .expect("failed to run pytest-rs")
    }

    fn run_with_env(&self, args: &[&str], env: &[(&str, &str)]) -> Output {
        std::process::Command::new(env!("CARGO_BIN_EXE_pytest-rs-bin"))
            .args(args)
            .current_dir(&self.root)
            .envs(env.iter().copied())
            .output()
            .expect("failed to run pytest-rs")
    }

    fn path(&self) -> &Path {
        &self.root
    }
}

impl Drop for TempSuite {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).to_string()
}

#[test]
fn basic_pass_fail_skip() {
    let suite = TempSuite::new("basic");
    suite.write(
        "test_basic.py",
        r#"
import pytest

def test_pass():
    assert 1 + 1 == 2

def test_fail():
    assert 1 + 1 == 3

@pytest.mark.skip(reason="nope")
def test_skip():
    raise AssertionError
"#,
    );
    let output = suite.run(&["test_basic.py"]);
    let out = stdout(&output);
    assert_eq!(output.status.code(), Some(1), "out: {out}");
    assert!(out.contains("1 failed, 1 passed, 1 skipped"), "out: {out}");
    assert!(
        out.contains("FAILED test_basic.py::test_fail"),
        "out: {out}"
    );
}

#[test]
fn fixture_scopes_and_teardown_order() {
    let suite = TempSuite::new("scopes");
    // Fixtures append events to a log file; the last test asserts the
    // session fixture was created exactly once and torn down generators ran.
    suite.write(
        "conftest.py",
        r#"
import pytest

COUNTS = {"session": 0, "module": 0}

@pytest.fixture(scope="session")
def session_fix():
    COUNTS["session"] += 1
    return COUNTS["session"]

@pytest.fixture(scope="module")
def module_fix():
    COUNTS["module"] += 1
    yield COUNTS["module"]
"#,
    );
    suite.write(
        "test_a.py",
        r#"
def test_a1(session_fix, module_fix):
    assert session_fix == 1
    assert module_fix == 1

def test_a2(session_fix, module_fix):
    assert session_fix == 1
    assert module_fix == 1
"#,
    );
    suite.write(
        "test_b.py",
        r#"
def test_b1(session_fix, module_fix):
    assert session_fix == 1
    assert module_fix == 2
"#,
    );
    let output = suite.run(&[]);
    let out = stdout(&output);
    assert_eq!(output.status.code(), Some(0), "out: {out}");
    assert!(out.contains("3 passed"), "out: {out}");
}

#[test]
fn generator_fixture_teardown_runs() {
    let suite = TempSuite::new("teardown");
    suite.write(
        "test_td.py",
        r#"
import pathlib
import pytest

@pytest.fixture
def thing():
    yield "value"
    pathlib.Path("teardown.txt").write_text("done")

def test_thing(thing):
    assert thing == "value"
"#,
    );
    let output = suite.run(&[]);
    let out = stdout(&output);
    assert_eq!(output.status.code(), Some(0), "out: {out}");
    assert_eq!(
        std::fs::read_to_string(suite.path().join("teardown.txt")).unwrap(),
        "done"
    );
}

#[test]
fn fixture_depends_on_fixture() {
    let suite = TempSuite::new("deps");
    suite.write(
        "test_deps.py",
        r#"
import pytest

@pytest.fixture
def base():
    return 10

@pytest.fixture
def derived(base):
    return base * 2

def test_derived(derived):
    assert derived == 20
"#,
    );
    let output = suite.run(&[]);
    assert_eq!(output.status.code(), Some(0), "out: {}", stdout(&output));
}

#[test]
fn raises_and_outcomes() {
    let suite = TempSuite::new("raises");
    suite.write(
        "test_raises.py",
        r#"
import pytest

def test_raises_ok():
    with pytest.raises(ValueError, match="bad"):
        raise ValueError("bad value")

def test_raises_callable():
    excinfo = pytest.raises(ZeroDivisionError, lambda: 1 / 0)
    assert excinfo.typename == "ZeroDivisionError"

def test_skip_inside():
    pytest.skip("not today")
"#,
    );
    let output = suite.run(&[]);
    let out = stdout(&output);
    assert_eq!(output.status.code(), Some(0), "out: {out}");
    assert!(out.contains("2 passed, 1 skipped"), "out: {out}");
}

#[test]
fn autouse_fixture() {
    let suite = TempSuite::new("autouse");
    suite.write(
        "test_autouse.py",
        r#"
import pytest

STATE = []

@pytest.fixture(autouse=True)
def prepare():
    STATE.append("ready")

def test_state():
    assert STATE == ["ready"]
"#,
    );
    let output = suite.run(&[]);
    assert_eq!(output.status.code(), Some(0), "out: {}", stdout(&output));
}

#[test]
fn collect_only_lists_nodeids() {
    let suite = TempSuite::new("collect");
    suite.write("test_co.py", "def test_one(): pass\ndef test_two(): pass\n");
    let output = suite.run(&["--collect-only", "-q"]);
    let out = stdout(&output);
    assert_eq!(output.status.code(), Some(0), "out: {out}");
    assert!(out.contains("test_co.py::test_one"), "out: {out}");
    assert!(out.contains("test_co.py::test_two"), "out: {out}");

    // Without -q, --collect-only renders the node tree.
    let output = suite.run(&["--collect-only"]);
    let out = stdout(&output);
    assert_eq!(output.status.code(), Some(0), "out: {out}");
    assert!(out.contains("<Module test_co.py>"), "out: {out}");
    assert!(out.contains("<Function test_one>"), "out: {out}");
    assert!(out.contains("2 tests collected"), "out: {out}");
}

#[test]
fn exitfirst_stops_after_failure() {
    let suite = TempSuite::new("exitfirst");
    suite.write(
        "test_x.py",
        r#"
def test_1():
    assert False

def test_2():
    assert True
"#,
    );
    let output = suite.run(&["-x"]);
    let out = stdout(&output);
    assert_eq!(output.status.code(), Some(1), "out: {out}");
    assert!(out.contains("1 failed"), "out: {out}");
    assert!(!out.contains("1 passed"), "out: {out}");
}

#[test]
fn no_tests_collected_exit_code() {
    let suite = TempSuite::new("empty");
    suite.write("test_empty.py", "X = 1\n");
    let output = suite.run(&[]);
    assert_eq!(output.status.code(), Some(5), "out: {}", stdout(&output));
}

#[test]
fn async_test_and_fixture_strict_marker() {
    let suite = TempSuite::new("asyncio");
    suite.write(
        "test_async.py",
        r#"
import asyncio
import pytest

@pytest.fixture
async def value():
    await asyncio.sleep(0)
    return 41

@pytest.fixture
async def gen_value():
    await asyncio.sleep(0)
    yield 1

@pytest.mark.asyncio
async def test_async(value, gen_value):
    await asyncio.sleep(0)
    assert value + gen_value == 42

async def test_async_unmarked_is_not_run():
    raise AssertionError("strict mode must not run this")
"#,
    );
    let output = suite.run(&[]);
    let out = stdout(&output);
    // pytest 9 parity: the plain async @pytest.fixture is unhandled in
    // strict mode (RemovedIn9 → error), and the unmarked async test fails
    // as unhandled (it used to be skipped).
    assert_eq!(output.status.code(), Some(1), "out: {out}");
    assert!(out.contains("requested an async fixture"), "out: {out}");
    assert!(
        out.contains("async def functions are not natively supported"),
        "out: {out}"
    );
}

#[test]
fn asyncio_auto_mode() {
    let suite = TempSuite::new("asyncio-auto");
    suite.write(
        "test_auto.py",
        r#"
import asyncio

async def test_async_auto():
    await asyncio.sleep(0)
    assert True
"#,
    );
    let output = suite.run(&["--asyncio-mode", "auto"]);
    let out = stdout(&output);
    assert_eq!(output.status.code(), Some(0), "out: {out}");
    assert!(out.contains("1 passed"), "out: {out}");
}

#[test]
fn parametrize_expands_items() {
    let suite = TempSuite::new("parametrize");
    suite.write(
        "test_params.py",
        r#"
import pytest

@pytest.mark.parametrize("a,b,expected", [(1, 2, 3), (2, 3, 5)])
def test_add(a, b, expected):
    assert a + b == expected

@pytest.mark.parametrize("x", [0, 1])
@pytest.mark.parametrize("y", [2, 3])
def test_stacked(x, y):
    assert x in (0, 1) and y in (2, 3)

@pytest.mark.parametrize("v", [pytest.param(9, id="nine"), pytest.param(0, marks=pytest.mark.skip)])
def test_param_obj(v):
    assert v == 9
"#,
    );
    let output = suite.run(&["-v"]);
    let out = stdout(&output);
    assert_eq!(output.status.code(), Some(0), "out: {out}");
    assert!(
        out.contains("test_params.py::test_add[1-2-3] PASSED"),
        "out: {out}"
    );
    assert!(
        out.contains("test_params.py::test_add[2-3-5] PASSED"),
        "out: {out}"
    );
    assert!(
        out.contains("test_params.py::test_stacked[2-0] PASSED"),
        "out: {out}"
    );
    assert!(
        out.contains("test_params.py::test_stacked[3-1] PASSED"),
        "out: {out}"
    );
    assert!(
        out.contains("test_params.py::test_param_obj[nine] PASSED"),
        "out: {out}"
    );
    assert!(
        out.contains("test_params.py::test_param_obj[0] SKIPPED"),
        "out: {out}"
    );
}

#[test]
fn test_class_collection() {
    let suite = TempSuite::new("classes");
    suite.write(
        "test_cls.py",
        r#"
import pytest

class TestThing:
    @pytest.fixture
    def offset(self):
        return 100

    def test_method(self, offset):
        assert offset + 1 == 101

    @pytest.mark.parametrize("v", [1, 2])
    def test_params(self, v, offset):
        assert offset + v > 100

class TestStateless:
    def test_fresh_instance(self):
        assert not hasattr(self, "state")
        self.state = True

class NotCollected:
    def test_ignored(self):
        raise AssertionError

class TestWithInit:
    def __init__(self):
        pass

    def test_skipped_class(self):
        raise AssertionError
"#,
    );
    let output = suite.run(&["-v"]);
    let out = stdout(&output);
    assert_eq!(output.status.code(), Some(0), "out: {out}");
    assert!(
        out.contains("test_cls.py::TestThing::test_method PASSED"),
        "out: {out}"
    );
    assert!(
        out.contains("test_cls.py::TestThing::test_params[1] PASSED"),
        "out: {out}"
    );
    assert!(out.contains("4 passed"), "out: {out}");
}

#[test]
fn fixture_params_and_request() {
    let suite = TempSuite::new("fixparams");
    suite.write(
        "test_fp.py",
        r#"
import pytest

@pytest.fixture(params=[1, 2, 3])
def number(request):
    return request.param

def test_number(number):
    assert number in (1, 2, 3)

@pytest.fixture
def with_finalizer(request):
    request.addfinalizer(lambda: open("fin.txt", "w").write("done"))
    return "v"

def test_finalizer(with_finalizer):
    assert with_finalizer == "v"

def test_request_node(request):
    assert "test_request_node" in request.node.nodeid
"#,
    );
    let output = suite.run(&["-v"]);
    let out = stdout(&output);
    assert_eq!(output.status.code(), Some(0), "out: {out}");
    assert!(
        out.contains("test_fp.py::test_number[1] PASSED"),
        "out: {out}"
    );
    assert!(
        out.contains("test_fp.py::test_number[3] PASSED"),
        "out: {out}"
    );
    assert!(out.contains("5 passed"), "out: {out}");
    assert_eq!(
        std::fs::read_to_string(suite.path().join("fin.txt")).unwrap(),
        "done"
    );
}

#[test]
fn skipif_and_xfail_marks() {
    let suite = TempSuite::new("skipif-xfail");
    suite.write(
        "test_sx.py",
        r#"
import sys
import pytest

@pytest.mark.skipif(sys.platform == "nowhere", reason="never true")
def test_runs():
    assert True

@pytest.mark.skipif("sys.platform != 'nowhere'", reason="string condition")
def test_skipped_by_string():
    raise AssertionError

@pytest.mark.xfail(reason="known bug")
def test_xfail():
    assert False

@pytest.mark.xfail
def test_xpass():
    assert True

def test_imperative_xfail():
    pytest.xfail("not implemented")
"#,
    );
    let output = suite.run(&["-v"]);
    let out = stdout(&output);
    assert_eq!(output.status.code(), Some(0), "out: {out}");
    assert!(out.contains("test_sx.py::test_runs PASSED"), "out: {out}");
    assert!(
        out.contains("test_sx.py::test_skipped_by_string SKIPPED"),
        "out: {out}"
    );
    assert!(out.contains("test_sx.py::test_xfail XFAIL"), "out: {out}");
    assert!(out.contains("test_sx.py::test_xpass XPASS"), "out: {out}");
    assert!(
        out.contains("test_sx.py::test_imperative_xfail XFAIL"),
        "out: {out}"
    );
    assert!(
        out.contains("1 passed, 1 skipped, 2 xfailed, 1 xpassed"),
        "out: {out}"
    );
}

#[test]
fn builtin_fixtures_tmp_path_monkeypatch() {
    let suite = TempSuite::new("builtins");
    suite.write(
        "test_builtin.py",
        r#"
import os

def test_tmp_path(tmp_path):
    f = tmp_path / "x.txt"
    f.write_text("hi")
    assert f.read_text() == "hi"

def test_monkeypatch_env(monkeypatch):
    monkeypatch.setenv("PYTEST_RS_TEST_ENV", "yes")
    assert os.environ["PYTEST_RS_TEST_ENV"] == "yes"

def test_env_undone():
    assert "PYTEST_RS_TEST_ENV" not in os.environ
"#,
    );
    let output = suite.run(&[]);
    let out = stdout(&output);
    assert_eq!(output.status.code(), Some(0), "out: {out}");
    assert!(out.contains("3 passed"), "out: {out}");
}

#[test]
fn pytester_runs_nested_session() {
    let suite = TempSuite::new("pytester");
    suite.write(
        "test_pt.py",
        r#"
def test_nested_run(pytester):
    pytester.makepyfile(
        """
        def test_inner_pass():
            assert True

        def test_inner_fail():
            assert False
        """
    )
    result = pytester.runpytest()
    assert result.ret == 1
    result.assert_outcomes(passed=1, failed=1)
    result.stdout.fnmatch_lines(["*1 failed, 1 passed*"])
"#,
    );
    let output = suite.run(&["-v"]);
    let out = stdout(&output);
    assert_eq!(output.status.code(), Some(0), "out: {out}");
    assert!(out.contains("1 passed"), "out: {out}");
}

#[test]
fn assertion_rewriting_shows_values() {
    let suite = TempSuite::new("rewrite");
    suite.write(
        "test_rw.py",
        r#"
def helper():
    return 41

def test_compare():
    assert helper() == 42

def test_with_message():
    assert 1 + 1 == 3, "math is broken"

def test_in():
    assert "x" in ["a", "b"]

def test_lineno_preserved():
    # failure must point at the next line
    value = None
    assert value is not None
"#,
    );
    let output = suite.run(&[]);
    let out = stdout(&output);
    assert_eq!(output.status.code(), Some(1), "out: {out}");
    assert!(out.contains("assert 41 == 42"), "out: {out}");
    assert!(out.contains("math is broken"), "out: {out}");
    assert!(out.contains("assert 'x' in ['a', 'b']"), "out: {out}");
    // traceback line number fidelity: `assert value is not None` is line 17
    // (the file content starts with a blank line from the raw string).
    // pytest-style location line + `>` marker on the failing source line.
    assert!(out.contains("test_rw.py:17: AssertionError"), "out: {out}");
    assert!(
        out.contains(">       assert value is not None"),
        "out: {out}"
    );
}

#[test]
fn tmp_path_factory_fixture() {
    let suite = TempSuite::new("tmpfactory");
    suite.write(
        "test_tf.py",
        r#"
def test_factory(tmp_path_factory):
    a = tmp_path_factory.mktemp("data")
    b = tmp_path_factory.mktemp("data")
    assert a != b
    assert a.name.startswith("data")
    assert a.is_dir()

def test_tmp_path_under_basetemp(tmp_path, tmp_path_factory):
    assert tmp_path.is_dir()
    assert tmp_path_factory.getbasetemp() in tmp_path.parents
"#,
    );
    let output = suite.run(&[]);
    let out = stdout(&output);
    assert_eq!(output.status.code(), Some(0), "out: {out}");
    assert!(out.contains("2 passed"), "out: {out}");
}

#[test]
fn ini_file_and_addopts() {
    let suite = TempSuite::new("ini");
    suite.write(
        "pytest.ini",
        "[pytest]\naddopts = -v\nasyncio_mode = auto\n",
    );
    suite.write(
        "test_ini.py",
        r#"
import asyncio

async def test_auto_from_ini():
    await asyncio.sleep(0)
"#,
    );
    // asyncio_mode=auto comes from pytest.ini; -v from addopts.
    let output = suite.run(&[]);
    let out = stdout(&output);
    assert_eq!(output.status.code(), Some(0), "out: {out}");
    assert!(
        out.contains("test_ini.py::test_auto_from_ini PASSED"),
        "out: {out}"
    );
}

#[test]
fn override_ini_beats_file() {
    let suite = TempSuite::new("ini-override");
    suite.write("pytest.ini", "[pytest]\nasyncio_mode = auto\n");
    suite.write(
        "test_o.py",
        r#"
async def test_async():
    raise AssertionError("must not run: strict via -o")
"#,
    );
    let output = suite.run(&["-o", "asyncio_mode=strict"]);
    let out = stdout(&output);
    // Strict (via -o, beating the ini's auto) fails the unmarked async test
    // as unhandled (pytest 9 parity); auto mode would have run its body.
    assert_eq!(output.status.code(), Some(1), "out: {out}");
    assert!(
        out.contains("async def functions are not natively supported"),
        "out: {out}"
    );
    assert!(!out.contains("must not run"), "out: {out}");
}

#[test]
fn warnings_captured_and_counted() {
    let suite = TempSuite::new("warnings");
    suite.write(
        "test_w.py",
        r#"
import warnings

def test_warns_once():
    warnings.warn("legacy api", DeprecationWarning)

def test_clean():
    assert True
"#,
    );
    let output = suite.run(&[]);
    let out = stdout(&output);
    assert_eq!(output.status.code(), Some(0), "out: {out}");
    assert!(out.contains("warnings summary"), "out: {out}");
    assert!(out.contains("DeprecationWarning: legacy api"), "out: {out}");
    assert!(out.contains("2 passed, 1 warning"), "out: {out}");
}

#[test]
fn w_error_turns_warning_into_failure() {
    let suite = TempSuite::new("werror");
    suite.write(
        "test_we.py",
        r#"
import warnings

def test_warns():
    warnings.warn("boom", UserWarning)
"#,
    );
    let output = suite.run(&["-W", "error"]);
    let out = stdout(&output);
    assert_eq!(output.status.code(), Some(1), "out: {out}");
    assert!(out.contains("1 failed"), "out: {out}");
}

#[test]
fn conftest_hooks_modifyitems_and_configure() {
    let suite = TempSuite::new("conftest-hooks");
    suite.write(
        "conftest.py",
        r#"
import pytest

CONFIGURED = []

def pytest_configure(config):
    CONFIGURED.append(config.rootpath)

@pytest.hookimpl(wrapper=True, tryfirst=True)
def pytest_collection_modifyitems(items):
    # reverse order and mark the first (originally last) test as skipped
    items[:] = list(reversed(items))
    items[0].add_marker(pytest.mark.skip)
    return (yield)
"#,
    );
    suite.write(
        "test_hooks.py",
        r#"
ORDER = []

def test_a():
    ORDER.append("a")

def test_b():
    ORDER.append("b")

def test_c():
    # runs first after the reversal; test_c itself is skipped
    raise AssertionError("must be skipped by conftest hook")
"#,
    );
    let output = suite.run(&["-v"]);
    let out = stdout(&output);
    assert_eq!(output.status.code(), Some(0), "out: {out}");
    assert!(out.contains("test_hooks.py::test_c SKIPPED"), "out: {out}");
    assert!(out.contains("2 passed, 1 skipped"), "out: {out}");
    // reversed order: c (skipped), b, a
    let pos_b = out.find("test_hooks.py::test_b").unwrap();
    let pos_a = out.find("test_hooks.py::test_a").unwrap();
    assert!(pos_b < pos_a, "expected b before a, out: {out}");
}

#[test]
fn unittest_testcase_collection() {
    let suite = TempSuite::new("unittest");
    suite.write(
        "test_ut.py",
        r#"
import unittest

class ThingTest(unittest.TestCase):
    def setUp(self):
        self.value = 41

    def test_passes(self):
        self.assertEqual(self.value + 1, 42)

    def test_fails(self):
        self.assertEqual(self.value, 0)

    @unittest.skip("not now")
    def test_skipped(self):
        raise AssertionError

    def test_skiptest_inside(self):
        raise unittest.SkipTest("dynamic skip")
"#,
    );
    let output = suite.run(&["-v"]);
    let out = stdout(&output);
    assert_eq!(output.status.code(), Some(1), "out: {out}");
    assert!(
        out.contains("test_ut.py::ThingTest::test_passes PASSED"),
        "out: {out}"
    );
    assert!(
        out.contains("test_ut.py::ThingTest::test_fails FAILED"),
        "out: {out}"
    );
    assert!(
        out.contains("test_ut.py::ThingTest::test_skipped SKIPPED"),
        "out: {out}"
    );
    assert!(out.contains("1 failed, 1 passed, 2 skipped"), "out: {out}");
}

#[test]
fn deselect_option() {
    let suite = TempSuite::new("deselect");
    suite.write(
        "test_des.py",
        r#"
import pytest

def test_one():
    pass

def test_two():
    pass

@pytest.mark.parametrize("x", [1, 2, 3])
def test_param(x):
    pass

class TestCls:
    def test_m1(self):
        pass
"#,
    );
    // Exact nodeid, one parametrized id, and a class prefix.
    let output = suite.run(&[
        "test_des.py",
        "--deselect",
        "test_des.py::test_two",
        "--deselect",
        "test_des.py::test_param[2]",
        "--deselect=test_des.py::TestCls",
    ]);
    let out = stdout(&output);
    assert_eq!(output.status.code(), Some(0), "out: {out}");
    assert!(out.contains("3 passed, 3 deselected"), "out: {out}");
    assert!(
        out.contains("collected 6 items / 3 deselected / 3 selected"),
        "out: {out}"
    );
}

#[test]
fn getfixturevalue_dynamic_resolution() {
    let suite = TempSuite::new("getfixturevalue");
    suite.write(
        "test_gfv.py",
        r#"
import pytest

@pytest.fixture
def base():
    return 10

@pytest.fixture
def derived(request):
    return request.getfixturevalue("base") + 1

def test_in_fixture(derived):
    assert derived == 11

def test_cache_identity(request, base):
    # dynamic resolution shares the cache with static injection
    assert request.getfixturevalue("base") is base

def test_missing(request):
    with pytest.raises(pytest.FixtureLookupError):
        request.getfixturevalue("nope")

class TestCls:
    @pytest.fixture
    def needs_self(self):
        return type(self).__name__

    def test_instance_bound(self, request):
        assert request.getfixturevalue("needs_self") == "TestCls"
"#,
    );
    let output = suite.run(&["test_gfv.py"]);
    let out = stdout(&output);
    assert_eq!(output.status.code(), Some(0), "out: {out}");
    assert!(out.contains("4 passed"), "out: {out}");
}

#[test]
fn mock_patch_args_not_fixtures() {
    let suite = TempSuite::new("mockpatch");
    suite.write(
        "test_patch.py",
        r#"
from unittest.mock import MagicMock, patch
import pytest

@pytest.fixture
def real_fixture():
    return "real"

@patch("os.getcwd")
@patch("os.getpid")
def test_stacked(mock_getpid, mock_getcwd, real_fixture):
    assert isinstance(mock_getpid, MagicMock)
    assert isinstance(mock_getcwd, MagicMock)
    assert real_fixture == "real"

@patch("os.getcwd", "replaced")
def test_patch_explicit_new(real_fixture):
    import os
    assert os.getcwd == "replaced"

class TestCls:
    @patch("os.getcwd")
    def test_method_patch(self, mock_getcwd, real_fixture):
        assert isinstance(mock_getcwd, MagicMock)
        assert real_fixture == "real"
"#,
    );
    let output = suite.run(&["test_patch.py"]);
    let out = stdout(&output);
    assert_eq!(output.status.code(), Some(0), "out: {out}");
    assert!(out.contains("3 passed"), "out: {out}");
}

#[test]
fn usefixtures_mark() {
    let suite = TempSuite::new("usefixtures");
    suite.write(
        "test_uf.py",
        r#"
import pytest

STATE = {}

@pytest.fixture
def setup_state():
    STATE["on"] = True
    yield
    STATE.pop("on", None)

@pytest.mark.usefixtures("setup_state")
def test_function_mark():
    assert STATE.get("on") is True

def test_no_mark():
    assert STATE.get("on") is None

@pytest.mark.usefixtures("setup_state")
class TestClassMark:
    def test_in_class(self):
        assert STATE.get("on") is True
"#,
    );
    let output = suite.run(&["test_uf.py"]);
    let out = stdout(&output);
    assert_eq!(output.status.code(), Some(0), "out: {out}");
    assert!(out.contains("3 passed"), "out: {out}");
}

#[test]
fn parametrize_trailing_comma_in_argnames() {
    let suite = TempSuite::new("trailing-comma");
    suite.write(
        "test_tc.py",
        r#"
import pytest

@pytest.mark.parametrize("a,b,", [(1, 2), (3, 4)])
def test_trailing(a, b):
    assert a + b in (3, 7)
"#,
    );
    let output = suite.run(&["-v"]);
    let out = stdout(&output);
    assert_eq!(output.status.code(), Some(0), "out: {out}");
    assert!(out.contains("2 passed"), "out: {out}");
}

#[test]
fn class_method_non_self_first_arg() {
    let suite = TempSuite::new("non-self");
    suite.write(
        "test_ns.py",
        r#"
class TestThing:
    def test_normal(self):
        assert True

    def test_weird_name(sel):
        assert True

    def test_another(me):
        assert True
"#,
    );
    let output = suite.run(&["-v"]);
    let out = stdout(&output);
    assert_eq!(output.status.code(), Some(0), "out: {out}");
    assert!(out.contains("3 passed"), "out: {out}");
}

#[test]
fn mark_object_in_pytest_param() {
    let suite = TempSuite::new("mark-param");
    suite.write(
        "test_mp.py",
        r#"
import pytest

skip_mark = pytest.mark.skip(reason="raw Mark object")

@pytest.mark.parametrize("x", [
    pytest.param(1, marks=skip_mark.mark),
    pytest.param(2),
])
def test_with_mark(x):
    assert x == 2
"#,
    );
    let output = suite.run(&["-v"]);
    let out = stdout(&output);
    assert_eq!(output.status.code(), Some(0), "out: {out}");
    assert!(out.contains("1 passed, 1 skipped"), "out: {out}");
}

#[test]
fn positional_only_class_method() {
    let suite = TempSuite::new("pos-only");
    suite.write(
        "test_pos.py",
        r#"
import pytest

class Test:
    @pytest.fixture
    def fix(self):
        return 42

    def test_method(self, /, fix):
        assert fix == 42
"#,
    );
    let output = suite.run(&["-v"]);
    let out = stdout(&output);
    assert_eq!(output.status.code(), Some(0), "out: {out}");
    assert!(out.contains("1 passed"), "out: {out}");
}

#[test]
fn yield_fixture_no_value() {
    let suite = TempSuite::new("yield-noval");
    suite.write(
        "test_yf.py",
        r#"
import pytest

@pytest.fixture(name="custom")
def empty_yield():
    if False:
        yield

def test_fixt(custom):
    pass
"#,
    );
    let output = suite.run(&["-v"]);
    let out = stdout(&output);
    assert_eq!(output.status.code(), Some(1), "out: {out}");
    assert!(out.contains("custom did not yield a value"), "out: {out}");
}

#[test]
fn raises_validation() {
    let suite = TempSuite::new("raises-val");
    suite.write(
        "test_rv.py",
        r#"
import pytest

def test_raises_not_callable():
    with pytest.raises(TypeError, match="must be callable"):
        pytest.raises(RuntimeError, "int('qwe')")

def test_raises_none():
    with pytest.raises(ValueError, match="at least one parameter"):
        pytest.raises(expected_exception=None)

def test_raises_false():
    with pytest.raises(ValueError, match="Expected an exception type"):
        pytest.raises(False, int)

def test_raises_exception_attr():
    assert pytest.raises.Exception is pytest.fail.Exception

def test_raises_did_not_raise_repr():
    try:
        pytest.raises(ValueError, int, "0")
    except pytest.fail.Exception as e:
        assert "DID NOT RAISE <class 'ValueError'>" in e.msg
    else:
        assert False, "Expected exception"

def test_match_format():
    with pytest.raises(AssertionError) as excinfo:
        with pytest.raises(ValueError, match="nope"):
            raise ValueError("actual")
    msg = str(excinfo.value)
    assert "Expected regex:" in msg
    assert "Actual message:" in msg
"#,
    );
    let output = suite.run(&["-v"]);
    let out = stdout(&output);
    assert_eq!(output.status.code(), Some(0), "out: {out}");
    assert!(out.contains("6 passed"), "out: {out}");
}

#[test]
fn abstract_class_not_collected() {
    let suite = TempSuite::new("abstract-cls");
    suite.write(
        "test_abs.py",
        r#"
import abc

class TestBase(abc.ABC):
    @abc.abstractmethod
    def abstract1(self): ...

    def test_it(self):
        pass

class TestConcrete(TestBase):
    def abstract1(self):
        return 1

    def test_it(self):
        assert self.abstract1() == 1
"#,
    );
    let output = suite.run(&["-v"]);
    let out = stdout(&output);
    assert_eq!(output.status.code(), Some(0), "out: {out}");
    assert!(out.contains("1 passed"), "out: {out}");
    assert!(
        !out.contains("TestBase"),
        "abstract class should not be collected: {out}"
    );
}

#[test]
fn dunder_test_false_skips_collection() {
    let suite = TempSuite::new("test-false");
    suite.write(
        "test_mod_false.py",
        r#"
__test__ = False
def test_should_not_run():
    assert False
"#,
    );
    suite.write(
        "test_items.py",
        r#"
def test_normal():
    pass

def test_skipped():
    pass
test_skipped.__test__ = False

class TestSkipped:
    __test__ = False
    def test_method(self):
        assert False

class TestNormal:
    def test_method(self):
        pass
"#,
    );
    let output = suite.run(&["-v"]);
    let out = stdout(&output);
    assert_eq!(output.status.code(), Some(0), "out: {out}");
    assert!(out.contains("2 passed"), "out: {out}");
    assert!(
        !out.contains("test_should_not_run"),
        "module __test__=False should skip: {out}"
    );
    assert!(
        !out.contains("test_skipped"),
        "function __test__=False should skip: {out}"
    );
    assert!(
        !out.contains("TestSkipped"),
        "class __test__=False should skip: {out}"
    );
}

#[test]
fn fixture_called_directly_raises() {
    let suite = TempSuite::new("fix-direct");
    suite.write(
        "test_fc.py",
        r#"
import pytest

@pytest.fixture
def fix():
    return 42

def test_direct_call():
    with pytest.raises(pytest.fail.Exception, match="called directly"):
        fix()

def test_via_request(fix):
    assert fix == 42
"#,
    );
    let output = suite.run(&["-v"]);
    let out = stdout(&output);
    assert_eq!(output.status.code(), Some(0), "out: {out}");
    assert!(out.contains("2 passed"), "out: {out}");
}

#[test]
fn virtualenv_pth_editable_install_is_importable() {
    // Regression test for #9: modern editable installs (setuptools>=64,
    // flit-core, ...) register importability via a .pth file that `import`s
    // a generated finder module, rather than a plain directory line. The
    // embedded interpreter never runs site.main() for a project venv on its
    // own, so this only works if activate_virtualenv's site.addsitedir call
    // actually processes .pth files (unlike a plain PYTHONPATH/sys.path append).
    let suite = TempSuite::new("venv-pth");
    let site_packages = "venv/lib/python3.13/site-packages";
    suite.write(
        &format!("{site_packages}/editable_finder.py"),
        r#"
import sys
import types

_mod = types.ModuleType("editable_pkg")
_mod.VALUE = 42
sys.modules["editable_pkg"] = _mod
"#,
    );
    suite.write(
        &format!("{site_packages}/editable_pkg.pth"),
        "import editable_finder\n",
    );
    suite.write(
        "test_editable.py",
        r#"
from editable_pkg import VALUE

def test_it():
    assert VALUE == 42
"#,
    );
    let venv_abs = suite.path().join("venv");
    let output = suite.run_with_env(
        &["test_editable.py"],
        &[("VIRTUAL_ENV", venv_abs.to_str().unwrap())],
    );
    let out = stdout(&output);
    assert_eq!(output.status.code(), Some(0), "out: {out}");
    assert!(out.contains("1 passed"), "out: {out}");
}

#[test]
fn xdist_worker_sees_subdir_conftest_addoption() {
    // Regression test: a conftest.py *below* the rootdir (not the rootdir's
    // own conftest, reached only once precollect starts walking that
    // subdirectory) that registers its own option in pytest_addoption, reads
    // it back via config.getoption() in pytest_configure, and stashes it as
    // a plain config attribute for pytest_collection_modifyitems to read —
    // an extremely common real-world pattern (e.g. networkx's own conftest.py
    // does exactly this with a --backend option). Under -n, a spawned worker
    // only ever fired pytest_addoption once, early, against whatever -p/
    // entry-point plugins had registered by then; this subdir conftest was
    // only imported later, incrementally, as precollect_all reached its
    // directory — so its own pytest_addoption never ran on the worker,
    // config.getoption("--mode") raised ValueError: no option named "--mode",
    // pytest_configure aborted before setting config.custom_mode, and
    // pytest_collection_modifyitems crashed on the missing attribute
    // (INTERNAL ERROR, "no tests ran"). Not caught by any existing
    // conformance suite, since none of them run a real project's own
    // conftest under -n (only pytest-xdist's own test suite, which doesn't
    // exercise this pattern).
    let suite = TempSuite::new("xdist-subdir-conftest-addoption");
    suite.write(
        "pkg/conftest.py",
        r#"
def pytest_addoption(parser):
    parser.addoption("--mode", action="store", default="fast")

def pytest_configure(config):
    config.custom_mode = config.getoption("--mode")

def pytest_collection_modifyitems(config, items):
    assert config.custom_mode == "fast"
"#,
    );
    suite.write(
        "pkg/test_thing.py",
        r#"
def test_a():
    assert 1 == 1

def test_b():
    assert 2 == 2
"#,
    );
    let output = suite.run(&["-n", "2", "pkg"]);
    let out = stdout(&output);
    let err = String::from_utf8_lossy(&output.stderr);
    assert!(!err.contains("INTERNAL ERROR"), "stderr: {err}\nout: {out}");
    assert!(!out.contains("no tests ran"), "out: {out}");
    assert_eq!(output.status.code(), Some(0), "out: {out}\nstderr: {err}");
    assert!(out.contains("2 passed"), "out: {out}");
}
