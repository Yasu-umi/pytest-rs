//! Config/node/session proxies and Python callable plumbing.

#[allow(unused_imports)]
use super::*;
use crate::collect::TestItem;
use pyo3::types::PyDict;

/// Build the Config proxy passed to conftest hooks. One proxy per process
/// (the Config itself is process-global), so attribute mutations made by
/// conftest hooks (e.g. `config.option.foo = ...`) stay visible everywhere.
pub(crate) static CONFIG_PROXY: std::sync::OnceLock<Py<PyAny>> = std::sync::OnceLock::new();

/// The process-global Config proxy, if one was built already.
pub fn existing_py_config(py: Python<'_>) -> Option<Py<PyAny>> {
    CONFIG_PROXY.get().map(|proxy| proxy.clone_ref(py))
}

pub fn make_py_config(py: Python<'_>, config: &crate::config::Config) -> PyResult<Py<PyAny>> {
    if let Some(proxy) = CONFIG_PROXY.get() {
        return Ok(proxy.clone_ref(py));
    }
    let proxy = build_py_config(py, config, false)?;
    Ok(CONFIG_PROXY.get_or_init(|| proxy).clone_ref(py))
}

/// `config.args`/`config.args_source`: where the collection arguments came
/// from. `config.paths` holds the raw CLI path tokens; an empty set falls
/// back to the `testpaths` ini, else to the invocation directory.
fn decide_args(config: &crate::config::Config) -> (Vec<String>, String) {
    if !config.paths.is_empty() {
        return (config.paths.clone(), "args".to_string());
    }
    if let Some(testpaths) = config.get_ini("testpaths")
        && !testpaths.trim().is_empty()
    {
        let args = testpaths.split_whitespace().map(str::to_string).collect();
        return (args, "testpaths".to_string());
    }
    (
        vec![config.invocation_dir.to_string_lossy().to_string()],
        "invocation_dir".to_string(),
    )
}

/// Build a fresh `Config` proxy (no singleton caching). Shared by the
/// session-global proxy and by `pytester.parseconfig`, which needs an
/// independent config built from its own args.
fn build_py_config(
    py: Python<'_>,
    config: &crate::config::Config,
    strict: bool,
) -> PyResult<Py<PyAny>> {
    // `config.option` is the argparse namespace in pytest; expose a mutable
    // namespace so conftests can stash flags on it. Unset names fall back
    // to plugin-registered option defaults (pytest._parser.OptionNamespace).
    let option_ns = py
        .import("pytest._parser")?
        .getattr("OptionNamespace")?
        .call0()?;
    // Native options plugins commonly read via config.option.<dest>
    // (sugar's print_failure checks option.tbstyle, etc.).
    // -q decrements like upstream's argparse count (-q → verbose -1).
    option_ns.setattr("verbose", config.verbose as i32 - config.quiet_level as i32)?;
    option_ns.setattr("quiet", config.quiet)?;
    option_ns.setattr("tbstyle", config.get_value("tb").unwrap_or("auto"))?;
    option_ns.setattr(
        "showcapture",
        config.get_value("show-capture").unwrap_or("all"),
    )?;
    option_ns.setattr("no_header", config.get_flag("no-header"))?;
    option_ns.setattr("no_summary", config.get_flag("no-summary"))?;
    option_ns.setattr("fulltrace", config.get_flag("full-trace"))?;
    option_ns.setattr("xfail_tb", config.get_flag("xfail-tb"))?;
    option_ns.setattr("traceconfig", false)?;
    option_ns.setattr("debug", false)?;
    option_ns.setattr(
        "capture",
        if config.get_flag("capture-disable") {
            "no"
        } else {
            config.get_value("capture").unwrap_or("fd")
        },
    )?;
    option_ns.setattr("color", config.get_value("color").unwrap_or("auto"))?;
    option_ns.setattr("collectonly", config.collect_only)?;
    if let Some(chars) = config.get_value("report-chars") {
        option_ns.setattr("reportchars", chars)?;
    }
    // Populate doctest-related option attributes so getoption() works.
    option_ns.setattr("doctest_modules", config.get_flag("doctest-modules"))?;
    option_ns.setattr(
        "doctest_continue_on_failure",
        config.get_flag("doctest-continue-on-failure"),
    )?;
    option_ns.setattr(
        "doctest_ignore_import_errors",
        config.get_flag("doctest-ignore-import-errors"),
    )?;
    option_ns.setattr(
        "doctest_report",
        config
            .get_value("doctest-report")
            .unwrap_or("none")
            .to_owned(),
    )?;
    // doctest_glob: list of glob patterns (multi-value)
    let glob_list = pyo3::types::PyList::empty(py);
    if let Some(globs) = config.get_values("doctest-glob") {
        for g in globs {
            glob_list.append(g)?;
        }
    }
    option_ns.setattr("doctest_glob", glob_list)?;
    let option = option_ns.unbind();
    let inipath = config
        .config_file_name
        .as_ref()
        .map(|name| config.rootdir.join(name).to_string_lossy().to_string());
    let (args, args_source) = decide_args(config);
    let proxy = Py::new(
        py,
        crate::request::PyConfig::new(
            config.rootdir.to_string_lossy().to_string(),
            inipath,
            args,
            args_source,
            config.ini_snapshot(),
            option,
            strict,
        ),
    )?
    .into_any();
    Ok(proxy)
}

/// `pytester.parseconfig(*args)`: build a fresh in-process Config from the
/// given command-line args (rootdir discovery, ini reading, option parsing),
/// without running a session. Raises `pytest.UsageError` on bad args,
/// matching upstream's `_prepareconfig`.
pub fn prepare_config(py: Python<'_>, args: Vec<String>) -> PyResult<Py<PyAny>> {
    let mut argv = vec!["pytest-rs".to_string()];
    argv.extend(args);
    let parser = crate::config::OptionParser::default();
    match crate::config::Config::from_args(parser, argv) {
        Ok(config) => build_py_config(py, &config, true),
        Err(message) => {
            let exc = py
                .import("pytest")?
                .getattr("UsageError")?
                .call1((message,))?;
            Err(PyErr::from_value(exc))
        }
    }
}

/// Build a `pytest._node.Node` for an item (used as `request.node`).
pub fn make_node(py: Python<'_>, item: &TestItem) -> PyResult<Py<PyAny>> {
    let marks = pyo3::types::PyList::empty(py);
    for mark in &item.marks {
        marks.append(mark.obj.bind(py))?;
    }
    let fixturenames = pyo3::types::PyList::new(
        py,
        item.fixture_names
            .iter()
            .chain(&item.extra_fixture_names)
            .collect::<Vec<_>>(),
    )?;
    // node.name is the last nodeid segment, parametrization id included
    // ("test_foo[a-1]"), matching pytest.
    let name = item
        .nodeid
        .rsplit("::")
        .next()
        .unwrap_or(item.func_name.as_str());
    let node_cls = if item.is_doctest {
        "DoctestNode"
    } else {
        "Function"
    };
    let node = py.import("pytest._node")?.getattr(node_cls)?.call1((
        item.nodeid.as_str(),
        name,
        marks,
        fixturenames,
        item.func.bind(py),
        item.path.to_string_lossy().as_ref(),
        item.lineno,
    ))?;
    // node.config: plugins reach the pluginmanager and stash through it
    // (e.g. pytest-timeout's item.config.pluginmanager.hook). The proxy is
    // initialized at configure time, well before any node exists.
    if let Some(proxy) = CONFIG_PROXY.get() {
        node.setattr("config", proxy.bind(py))?;
    }
    // node.module / node.cls: reordering plugins (pytest-randomly,
    // pytest-order) shuffle by item.module.__name__ and item.cls. The module
    // was imported into sys.modules during collection; the class is carried
    // on the TestItem for methods of a Test* class.
    let module = py
        .import("sys")?
        .getattr("modules")?
        .call_method1("get", (item.module_name.as_str(),))?;
    node.setattr("module", module)?;
    // For pytest-style classes the class is on TestItem.cls; unittest items
    // keep cls None (see collect_testcase) and expose it on the shim runner.
    let cls = match &item.cls {
        Some(cls) => Some(cls.bind(py).clone()),
        None => {
            let func = item.func.bind(py);
            if func.hasattr("make_case").unwrap_or(false) {
                func.getattr("cls").ok()
            } else {
                None
            }
        }
    };
    if let Some(cls) = cls {
        node.setattr("cls", cls)?;
    }
    // node.location = (relpath, 0-based lineno, domain) — pytest-rerunfailures
    // passes it to pytest_runtest_logstart/logfinish.
    let file = item.nodeid.split("::").next().unwrap_or("");
    let domain = item
        .nodeid
        .split_once("::")
        .map(|(_, rest)| rest.replace("::", "."))
        .unwrap_or_default();
    node.setattr("location", (file, item.lineno.saturating_sub(1), domain))?;
    Ok(node.unbind())
}

/// Plugin-set session.shouldfail message (pytest._node._session_state),
/// polled by the runner between items.
pub fn session_shouldfail(py: Python<'_>) -> Option<String> {
    py.import("pytest._node")
        .and_then(|m| m.call_method0("session_shouldfail"))
        .ok()?
        .extract()
        .ok()?
}

/// Publish a truthy session.shouldfail to the Python session state so the
/// conftest's pytest_sessionfinish sees it (--maxfail). Engine-side set
/// bypasses the sticky setter.
pub fn set_session_shouldfail(py: Python<'_>, message: &str) {
    let _ = py
        .import("pytest._node")
        .and_then(|m| m.call_method1("set_session_shouldfail", (message,)));
}

/// A reason a test/plugin set on `session.shouldstop` (pytest banners it at
/// the end of the run with `! <reason> !`).
pub fn session_shouldstop(py: Python<'_>) -> Option<String> {
    py.import("pytest._node")
        .and_then(|m| m.call_method0("session_shouldstop"))
        .ok()?
        .extract()
        .ok()?
}

/// Publish a truthy session.shouldstop (--stepwise).
pub fn set_session_shouldstop(py: Python<'_>, message: &str) {
    let _ = py
        .import("pytest._node")
        .and_then(|m| m.call_method1("set_session_shouldstop", (message,)));
}

/// Evaluate a (skipif) condition string in a test module's namespace.
pub fn eval_in_module(py: Python<'_>, module_name: &str, expr: &str) -> PyResult<bool> {
    let module = py.import(module_name)?;
    let globals = module.dict();
    let code = std::ffi::CString::new(expr)
        .map_err(|e| pyo3::exceptions::PyValueError::new_err(e.to_string()))?;
    py.eval(code.as_c_str(), Some(&globals), None)?.is_truthy()
}

/// The Session proxy passed to pytest_sessionstart /
/// pytest_collection_finish python hooks (config + shouldfail + items).
pub fn make_session_proxy(py: Python<'_>, config: &crate::config::Config) -> PyResult<Py<PyAny>> {
    let config_proxy = make_py_config(py, config)?;
    Ok(py
        .import("pytest._node")?
        .getattr("_NodeSession")?
        .call1((config_proxy,))?
        .unbind())
}

/// Publish the collected items on the session proxy (session.items /
/// session.testscollected), once collection settles.
pub fn set_session_items(py: Python<'_>, items: &[crate::collect::TestItem]) -> PyResult<()> {
    let nodes = pyo3::types::PyList::empty(py);
    for item in items {
        nodes.append(make_node(py, item)?)?;
    }
    py.import("pytest._node")?
        .getattr("set_session_items")?
        .call1((nodes,))?;
    Ok(())
}

/// Write back `item.obj` swaps a plugin made on the published session
/// items (pytest-run-parallel wraps test functions for threaded repeats
/// during pytest_collection_finish).
pub fn apply_session_obj_overrides(py: Python<'_>, items: &mut [TestItem]) -> PyResult<()> {
    let overrides: Vec<(String, Py<PyAny>)> = py
        .import("pytest._node")?
        .call_method0("session_obj_overrides")?
        .extract()?;
    if overrides.is_empty() {
        return Ok(());
    }
    let by_nodeid: std::collections::HashMap<String, Py<PyAny>> = overrides.into_iter().collect();
    for item in items.iter_mut() {
        if let Some(obj) = by_nodeid.get(&item.nodeid) {
            item.func = obj.clone_ref(py);
        }
    }
    Ok(())
}

/// Call a Python callable with keyword arguments resolved from fixtures.
pub fn call_with_kwargs<'py>(
    py: Python<'py>,
    func: &Py<PyAny>,
    kwargs: &[(String, Py<PyAny>)],
) -> PyResult<Bound<'py, PyAny>> {
    call_fixture(py, func, None, kwargs)
}

/// Call a fixture/test function, prepending the test class instance as
/// `self` when the definition lives inside a Test* class. Calls run in the
/// current item's contextvars context (pytest._ctx) so vars set by
/// fixtures propagate into the test.
pub fn call_fixture<'py>(
    py: Python<'py>,
    func: &Py<PyAny>,
    instance: Option<&Py<PyAny>>,
    kwargs: &[(String, Py<PyAny>)],
) -> PyResult<Bound<'py, PyAny>> {
    let dict = PyDict::new(py);
    for (name, value) in kwargs {
        dict.set_item(name, value.bind(py))?;
    }
    let call = py.import("pytest._ctx")?.getattr("call")?;
    match instance {
        Some(instance) => call.call((func.bind(py), instance.bind(py)), Some(&dict)),
        None => call.call((func.bind(py),), Some(&dict)),
    }
}

/// Begin/end the per-item contextvars context.
pub fn begin_item_context(py: Python<'_>) -> PyResult<()> {
    py.import("pytest._ctx")?.call_method0("begin_item")?;
    Ok(())
}

pub fn end_item_context(py: Python<'_>) {
    let _ = py
        .import("pytest._ctx")
        .and_then(|m| m.call_method0("end_item"));
}

/// Resume a suspended sync generator fixture, expecting StopIteration.
/// Runs in the item context so contextvar tokens reset cleanly.
pub fn finalize_generator(py: Python<'_>, generator: &Py<PyAny>) -> PyResult<()> {
    let next_fn = py.import("builtins")?.getattr("next")?;
    let call = py.import("pytest._ctx")?.getattr("call")?;
    match call.call1((next_fn, generator.bind(py))) {
        Ok(_) => Err(pyo3::exceptions::PyRuntimeError::new_err(
            "fixture generator yielded more than once",
        )),
        Err(err) if err.is_instance_of::<pyo3::exceptions::PyStopIteration>(py) => Ok(()),
        Err(err) => Err(err),
    }
}

/// Advance a generator fixture to its first yield, returning the value.
/// Runs in the item context so contextvars set before the yield propagate.
pub fn next_value<'py>(
    py: Python<'py>,
    generator: &Bound<'py, PyAny>,
) -> PyResult<Bound<'py, PyAny>> {
    let next_fn = py.import("builtins")?.getattr("next")?;
    py.import("pytest._ctx")?
        .getattr("call")?
        .call1((next_fn, generator))
}
