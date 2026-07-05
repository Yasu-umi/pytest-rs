//! Config/node/session proxies and Python callable plumbing.

#[allow(unused_imports)]
use super::*;
use crate::collect::TestItem;
use pyo3::types::PyDict;

/// Build the Config proxy passed to conftest hooks. One proxy per process
/// (the Config itself is process-global), so attribute mutations made by
/// conftest hooks (e.g. `config.option.foo = ...`) stay visible everywhere.
pub(crate) static CONFIG_PROXY: std::sync::OnceLock<Py<PyAny>> = std::sync::OnceLock::new();

// Cached references to hot-path Python callables. Initialized on first call;
// stable for the lifetime of the process (module-level functions don't change).
static CTX_CALL: pyo3::sync::PyOnceLock<Py<PyAny>> = pyo3::sync::PyOnceLock::new();
static CTX_BEGIN_ITEM: pyo3::sync::PyOnceLock<Py<PyAny>> = pyo3::sync::PyOnceLock::new();
static CTX_END_ITEM: pyo3::sync::PyOnceLock<Py<PyAny>> = pyo3::sync::PyOnceLock::new();
static BUILTINS_NEXT: pyo3::sync::PyOnceLock<Py<PyAny>> = pyo3::sync::PyOnceLock::new();
static FIXTURES_FINALIZE: pyo3::sync::PyOnceLock<Py<PyAny>> = pyo3::sync::PyOnceLock::new();

fn ctx_call(py: Python<'_>) -> PyResult<Bound<'_, PyAny>> {
    CTX_CALL
        .get_or_try_init(py, || {
            Ok(py.import("pytest._ctx")?.getattr("call")?.unbind())
        })
        .map(|f| f.bind(py).clone())
}

fn ctx_begin_item(py: Python<'_>) -> PyResult<Bound<'_, PyAny>> {
    CTX_BEGIN_ITEM
        .get_or_try_init(py, || {
            Ok(py.import("pytest._ctx")?.getattr("begin_item")?.unbind())
        })
        .map(|f| f.bind(py).clone())
}

fn ctx_end_item(py: Python<'_>) -> PyResult<Bound<'_, PyAny>> {
    CTX_END_ITEM
        .get_or_try_init(py, || {
            Ok(py.import("pytest._ctx")?.getattr("end_item")?.unbind())
        })
        .map(|f| f.bind(py).clone())
}

fn builtins_next(py: Python<'_>) -> PyResult<Bound<'_, PyAny>> {
    BUILTINS_NEXT
        .get_or_try_init(py, || Ok(py.import("builtins")?.getattr("next")?.unbind()))
        .map(|f| f.bind(py).clone())
}

fn fixtures_finalize_fn(py: Python<'_>) -> PyResult<Bound<'_, PyAny>> {
    FIXTURES_FINALIZE
        .get_or_try_init(py, || {
            Ok(py
                .import("_pytest.fixtures")?
                .getattr("finalize_generator")?
                .unbind())
        })
        .map(|f| f.bind(py).clone())
}

thread_local! {
    /// Config proxies for active in-process nested runs (a stack for
    /// re-entrancy). While non-empty, the top shadows the process-global
    /// CONFIG_PROXY so a nested run sees its own config/ini values instead of
    /// the outer run's cached singleton (e.g. getini("markers") read from the
    /// nested run's tox.ini).
    static NESTED_CONFIG_PROXY: std::cell::RefCell<Vec<Py<PyAny>>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// Build a config proxy for a nested run and push it as the active one. The
/// returned guard pops it when the nested run ends.
pub(crate) fn push_nested_config_proxy(
    py: Python<'_>,
    config: &crate::config::Config,
) -> PyResult<NestedConfigGuard> {
    let proxy = build_py_config(py, config, false)?;
    NESTED_CONFIG_PROXY.with(|stack| stack.borrow_mut().push(proxy));
    Ok(NestedConfigGuard)
}

pub(crate) struct NestedConfigGuard;

impl Drop for NestedConfigGuard {
    fn drop(&mut self) {
        NESTED_CONFIG_PROXY.with(|stack| {
            stack.borrow_mut().pop();
        });
    }
}

fn nested_config_proxy(py: Python<'_>) -> Option<Py<PyAny>> {
    NESTED_CONFIG_PROXY.with(|stack| stack.borrow().last().map(|proxy| proxy.clone_ref(py)))
}

/// The active Config proxy, if one was built already (nested run's shadows
/// the process-global one).
pub fn existing_py_config(py: Python<'_>) -> Option<Py<PyAny>> {
    nested_config_proxy(py).or_else(|| CONFIG_PROXY.get().map(|proxy| proxy.clone_ref(py)))
}

pub fn make_py_config(py: Python<'_>, config: &crate::config::Config) -> PyResult<Py<PyAny>> {
    if let Some(proxy) = nested_config_proxy(py) {
        return Ok(proxy);
    }
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
    option_ns.setattr("traceconfig", config.get_flag("traceconfig"))?;
    option_ns.setattr("debug", false)?;
    option_ns.setattr("usepdb", config.get_flag("pdb"))?;
    option_ns.setattr("trace", config.get_flag("trace"))?;
    // --pdbcls=modname:classname → tuple (modname, classname) or None
    if let Some(pdbcls) = config.get_value("pdbcls") {
        if let Some((modname, classname)) = pdbcls.split_once(':') {
            let tuple = pyo3::types::PyTuple::new(py, [modname, classname])?;
            option_ns.setattr("usepdb_cls", tuple)?;
        } else {
            option_ns.setattr("usepdb_cls", py.None())?;
        }
    } else {
        option_ns.setattr("usepdb_cls", py.None())?;
    }
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
    // --fixtures / --funcargs both set config.option.showfixtures (upstream dest).
    option_ns.setattr("showfixtures", config.get_flag("fixtures"))?;
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
    // confcutdir is a CLI option plugins read via getoption("confcutdir").
    if let Some(confcutdir) = config.get_value("confcutdir") {
        option_ns.setattr("confcutdir", confcutdir.to_owned())?;
    }
    // -p plugin specs (config.option.plugins in upstream pytest).
    let plugins_list = pyo3::types::PyList::empty(py);
    for p in &config.plugin_opts {
        plugins_list.append(p)?;
    }
    option_ns.setattr("plugins", plugins_list)?;
    option_ns.setattr(
        "disable_plugin_autoload",
        config.get_flag("disable-plugin-autoload"),
    )?;
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
            config.ini_file_clone(),
            config.ini_overrides_clone(),
            option,
            strict,
        ),
    )?
    .into_any();
    // config.known_args_namespace: pytest's parsed argparse namespace, an
    // alias of config.option (pytest-env reads it for its verbose flag).
    let bound = proxy.bind(py);
    let opt = bound.getattr("option")?;
    bound.setattr("known_args_namespace", opt)?;
    // config.invocation_params.args: the verbatim args passed to
    // pytest.main()/the CLI (before addopts splicing).
    bound.call_method1("_set_invocation_args", (&config.invocation_args,))?;
    // config.invocation_params.plugins: extra plugin objects passed to this
    // invocation (pytester.inline_run(..., plugins=[...])), staged by
    // pytest._pytester in a module-level list before the nested run starts;
    // empty for every other config build (normal CLI run, parseconfig).
    let invocation_plugins = py
        .import("pytest._pytester")?
        .getattr("_pending_invocation_plugins")?;
    bound.call_method1("_set_invocation_plugins", (invocation_plugins,))?;
    // config.pluginmanager._confcutdir: the directory below which conftest.py
    // files are not considered (--confcutdir if given, else rootdir).
    let confcutdir_str = config
        .get_value("confcutdir")
        .map(|s| s.to_string())
        .unwrap_or_else(|| config.rootdir.to_string_lossy().to_string());
    let pathlib = py.import("pathlib")?;
    let confcutdir_path = pathlib.getattr("Path")?.call1((confcutdir_str,))?;
    let pm = bound.getattr("pluginmanager")?;
    pm.setattr("_confcutdir", confcutdir_path)?;
    pm.setattr("_config", &proxy)?;
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
        Ok(config) => {
            let proxy = build_py_config(py, &config, true)?;
            Ok(proxy)
        }
        Err(message) if message.starts_with(crate::EXIT_ZERO_SENTINEL) => {
            // --help/--version in-process: raise SystemExit(0) so the caller
            // can catch it (real pytest raises SystemExit from argparse too).
            Err(PyErr::new::<pyo3::exceptions::PySystemExit, _>(0_i32))
        }
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
    // Custom collector items: the func IS the pytest.Item node (with its own
    // nodeid/markers/config); expose it directly so isinstance checks and
    // session.items aggregation (pytest-mypy) see the real object.
    if py
        .import("pytest._node")
        .and_then(|m| m.getattr("Item"))
        .and_then(|cls| item.func.bind(py).is_instance(&cls))
        .unwrap_or(false)
    {
        return Ok(item.func.clone_ref(py));
    }
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
    // If a Python node was already created by pytest_pycollect_makeitem (e.g.
    // a custom Function subclass with overridden reportinfo or extra attrs set
    // by a wrapper hook), reuse it directly.  Otherwise build a fresh node.
    let node = if let Some(existing) = item.py_node.as_ref() {
        let n = existing.bind(py).clone();
        // The Python node was created at collection time with empty fixture
        // lists; update them now that Rust has resolved the full fixture
        // closure so the runner can fill fixtures correctly.
        n.setattr("fixturenames", &fixturenames)?;
        n
    } else {
        let node_cls = if item.is_doctest {
            "DoctestNode"
        } else {
            "Function"
        };
        py.import("pytest._node")?.getattr(node_cls)?.call1((
            item.nodeid.as_str(),
            name,
            marks,
            fixturenames,
            item.func.bind(py),
            item.path.to_string_lossy().as_ref(),
            item.lineno,
        ))?
    };
    // node.config: plugins reach the pluginmanager and stash through it
    // (e.g. pytest-timeout's item.config.pluginmanager.hook). The proxy is
    // initialized at configure time, well before any node exists.
    if let Some(proxy) = existing_py_config(py) {
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
    // node.callspec for parametrized items: params dict + the "[a-1]" id from
    // the nodeid.  Both direct parametrize and indirect (fixture) parametrize
    // produce a "[...]" suffix; pandas reads request.node.callspec.id even when
    // all parameters are indirect (callspec.params may be empty).
    if let Some((_, bracket_id)) = item.nodeid.rsplit_once('[') {
        let params = pyo3::types::PyDict::new(py);
        for (name, value) in &item.callspec {
            params.set_item(name, value.bind(py))?;
        }
        let id = bracket_id.trim_end_matches(']');
        let callspec = py
            .import("pytest._node")?
            .getattr("_CallSpec")?
            .call1((params, id))?;
        node.setattr("callspec", callspec)?;
    }
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

/// Set the testscollected count override (xdist: items are empty on the
/// controller, but the true count is known from worker reports).
pub fn set_session_testscollected(py: Python<'_>, n: usize) -> PyResult<()> {
    py.import("pytest._node")?
        .getattr("set_session_testscollected")?
        .call1((n,))?;
    Ok(())
}

/// Publish skipped-module records [(nodeid, reason, location), ...] on the
/// session state so the relay plugin can serialize them in collection_finish.
pub fn set_session_skipped_modules(
    py: Python<'_>,
    modules: &[(String, String, String)],
) -> PyResult<()> {
    let list = pyo3::types::PyList::empty(py);
    for (nodeid, reason, location) in modules {
        let tup =
            pyo3::types::PyTuple::new(py, [nodeid.as_str(), reason.as_str(), location.as_str()])?;
        list.append(tup)?;
    }
    py.import("pytest._node")?
        .getattr("set_session_skipped_modules")?
        .call1((list,))?;
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
    let call = ctx_call(py)?;
    match instance {
        Some(instance) => call.call((func.bind(py), instance.bind(py)), Some(&dict)),
        None => call.call((func.bind(py),), Some(&dict)),
    }
}

/// Begin/end the per-item contextvars context.
pub fn begin_item_context(py: Python<'_>) -> PyResult<()> {
    ctx_begin_item(py)?.call0()?;
    Ok(())
}

pub fn end_item_context(py: Python<'_>) {
    let _ = ctx_end_item(py).and_then(|f| f.call0());
}

/// Resume a suspended sync generator fixture, expecting StopIteration.
/// Runs in the item context so contextvar tokens reset cleanly.
pub fn finalize_generator(py: Python<'_>, generator: &Py<PyAny>) -> PyResult<()> {
    // _pytest.fixtures.finalize_generator advances the generator and, on a
    // second yield, reports it like upstream's fail_fixturefunc (message +
    // location, no traceback). Run it in the item context so contextvar
    // tokens set before the yield reset cleanly.
    let call = ctx_call(py)?;
    call.call1((fixtures_finalize_fn(py)?, generator.bind(py)))?;
    Ok(())
}

/// Advance a generator fixture to its first yield, returning the value.
/// Runs in the item context so contextvars set before the yield propagate.
pub fn next_value<'py>(
    py: Python<'py>,
    generator: &Bound<'py, PyAny>,
) -> PyResult<Bound<'py, PyAny>> {
    ctx_call(py)?.call1((builtins_next(py)?, generator))
}

/// Snapshot the shim pluginmanager's plugin list so a nested run's conftest
/// registrations can be rolled back. Returns a guard that restores the
/// original list on drop.
///
/// Also unregisters 'terminalreporter': a real subprocess-backed nested run
/// starts with a brand-new plugin manager that has never seen it, so a
/// nested `-p`/conftest pytest_configure hook querying
/// `get_plugin("terminalreporter")` should see it as absent until the
/// nested run's own configure phase re-registers it (upstream pluggy LIFO
/// ordering). The shim's pluginmanager is a single process-wide singleton,
/// so without this the nested run would see the outer run's registration
/// left over from before this guard was taken. Restored on drop regardless
/// of what the nested run did to it.
pub(crate) fn snapshot_pluginmanager(py: Python<'_>) -> PyResult<PluginManagerGuard> {
    let pm = py
        .import("pytest._pluginmanager")?
        .getattr("pluginmanager")?;
    let plugins: Py<PyAny> = pm.getattr("_plugins")?.call_method0("copy")?.unbind();
    let conftest_plugins: Py<PyAny> = pm
        .getattr("_conftest_plugins")?
        .call_method0("copy")?
        .unbind();
    let names: Py<PyAny> = pm.getattr("_names")?.call_method0("copy")?.unbind();
    pm.call_method1("unregister", (py.None(), "terminalreporter"))?;
    Ok(PluginManagerGuard {
        plugins,
        conftest_plugins,
        names,
    })
}

pub(crate) struct PluginManagerGuard {
    plugins: Py<PyAny>,
    conftest_plugins: Py<PyAny>,
    names: Py<PyAny>,
}

impl Drop for PluginManagerGuard {
    fn drop(&mut self) {
        Python::try_attach(|py| {
            if let Ok(pm) = py
                .import("pytest._pluginmanager")
                .and_then(|m| m.getattr("pluginmanager"))
            {
                let _ = pm.setattr("_plugins", self.plugins.bind(py));
                let _ = pm.setattr("_conftest_plugins", self.conftest_plugins.bind(py));
                let _ = pm.setattr("_names", self.names.bind(py));
            }
        });
    }
}
