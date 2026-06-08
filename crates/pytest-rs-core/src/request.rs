//! The Python-visible `request` fixture and `config` proxy objects.

use std::collections::HashMap;
use std::sync::Mutex;

use pyo3::prelude::*;

/// A (subset of the) pytest `Config` API passed to conftest hooks.
/// `dict`: plugins set ad-hoc attributes on config (pytest-timeout's
/// `config._env_timeout`), like upstream's plain-Python Config.
#[pyclass(name = "Config", dict)]
pub struct PyConfig {
    rootdir: String,
    /// Full path to the discovered config file (pytest.ini / pyproject.toml
    /// / tox.ini / setup.cfg), or None when no config file was found.
    inipath: Option<String>,
    /// The resolved collection arguments (`config.args`): the path-like CLI
    /// tokens, or the testpaths/invocation-dir fallback.
    args: Vec<String>,
    /// `config.args_source`: "args" (explicit CLI paths), "testpaths" (from
    /// the testpaths ini), or "invocation_dir" (the default).
    args_source: String,
    ini: Mutex<HashMap<String, String>>,
    /// Strict getini: an unregistered, non-core ini key raises ValueError
    /// (upstream behavior). Only parseconfig-built configs are strict; the
    /// session config stays lenient since the engine owns the core inis.
    strict: bool,
    /// The argparse-namespace equivalent (`config.option`), mutable from
    /// Python so conftest hooks can stash flags on it.
    #[pyo3(get)]
    option: Py<PyAny>,
    /// Lazily-created `pytest.Stash`; the config proxy is a session
    /// singleton, so plugin data stored here (e.g. pytest-timeout's
    /// session deadline) persists across hooks.
    stash: pyo3::sync::PyOnceLock<Py<PyAny>>,
}

impl PyConfig {
    pub fn new(
        rootdir: String,
        inipath: Option<String>,
        args: Vec<String>,
        args_source: String,
        ini: HashMap<String, String>,
        option: Py<PyAny>,
        strict: bool,
    ) -> Self {
        Self {
            rootdir,
            inipath,
            args,
            args_source,
            ini: Mutex::new(ini),
            strict,
            option,
            stash: pyo3::sync::PyOnceLock::new(),
        }
    }
}

#[pymethods]
impl PyConfig {
    #[getter]
    fn stash(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        Ok(self
            .stash
            .get_or_try_init(py, || -> PyResult<Py<PyAny>> {
                Ok(py
                    .import("pytest._stash")?
                    .getattr("Stash")?
                    .call0()?
                    .unbind())
            })?
            .clone_ref(py))
    }

    /// pytest's Config.issue_config_time_warning: a warning raised during
    /// configure (no test to attribute it to); the session warning capture
    /// records it for the warnings summary.
    #[pyo3(signature = (warning, stacklevel = 2))]
    fn issue_config_time_warning(
        &self,
        py: Python<'_>,
        warning: Py<PyAny>,
        stacklevel: i32,
    ) -> PyResult<()> {
        let kwargs = pyo3::types::PyDict::new(py);
        kwargs.set_item("stacklevel", stacklevel)?;
        py.import("warnings")?
            .call_method("warn", (warning,), Some(&kwargs))?;
        Ok(())
    }

    fn getini(&self, py: Python<'_>, name: &str) -> PyResult<Py<PyAny>> {
        // Pass the full ini snapshot so the resolver can apply alias lookups
        // and type coercion (parser.addini specs supply both); paths/pathlist
        // types resolve relative to rootdir.
        let inicfg = pyo3::types::PyDict::new(py);
        {
            let ini = self.ini.lock().expect("config lock poisoned");
            for (key, value) in ini.iter() {
                inicfg.set_item(key, value)?;
            }
        }
        Ok(py
            .import("pytest._parser")?
            .call_method1("getini", (name, inicfg, self.rootdir.as_str(), self.strict))?
            .unbind())
    }

    /// `config._get_unknown_ini_keys()`: ini-file keys that aren't a
    /// registered or core option (pytest's unknown-config-option set).
    fn _get_unknown_ini_keys(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        let keys: Vec<String> = self
            .ini
            .lock()
            .expect("config lock poisoned")
            .keys()
            .cloned()
            .collect();
        Ok(py
            .import("pytest._parser")?
            .call_method1("unknown_ini_keys", (keys,))?
            .unbind())
    }

    /// Append one line to a line-list ini option (e.g. "markers").
    fn addinivalue_line(&self, name: &str, line: &str) {
        let mut ini = self.ini.lock().expect("config lock poisoned");
        let entry = ini.entry(name.to_string()).or_default();
        if !entry.is_empty() {
            entry.push('\n');
        }
        entry.push_str(line);
    }

    #[pyo3(signature = (name, default = None))]
    fn getoption(
        &self,
        py: Python<'_>,
        name: &str,
        default: Option<Py<PyAny>>,
    ) -> PyResult<Py<PyAny>> {
        // Normalize: "--foo-bar" → "foo_bar", "foo-bar" → "foo_bar"
        let attr = name.trim_start_matches('-').replace('-', "_");
        let ns = self.option.bind(py);
        if let Ok(v) = ns.getattr(attr.as_str()) {
            return Ok(v.into());
        }
        // Plugin-registered options (parser.addoption) behave as if parsed
        // with their declared default; the default= argument only covers
        // unregistered names (pytest semantics).
        let (registered, value): (bool, Py<PyAny>) = py
            .import("pytest._parser")?
            .call_method1("option_lookup", (attr.as_str(),))?
            .extract()?;
        if registered {
            return Ok(value);
        }
        Ok(default.unwrap_or_else(|| py.None()))
    }

    #[pyo3(signature = (name, default = None))]
    fn getvalue(
        &self,
        py: Python<'_>,
        name: &str,
        default: Option<Py<PyAny>>,
    ) -> PyResult<Py<PyAny>> {
        self.getoption(py, name, default)
    }

    /// `config.getvalueorskip(name)`: the option's value, or skip the test if
    /// it is unset/None (pytest's getoption(..., skip=True)).
    fn getvalueorskip(&self, py: Python<'_>, name: &str) -> PyResult<Py<PyAny>> {
        let attr = name.trim_start_matches('-').replace('-', "_");
        if let Ok(v) = self.option.bind(py).getattr(attr.as_str())
            && !v.is_none()
        {
            return Ok(v.unbind());
        }
        let (registered, value): (bool, Py<PyAny>) = py
            .import("pytest._parser")?
            .call_method1("option_lookup", (attr.as_str(),))?
            .extract()?;
        if registered && !value.bind(py).is_none() {
            return Ok(value);
        }
        py.import("pytest")?
            .getattr("skip")?
            .call1((format!("no {attr:?} option found"),))?;
        Ok(py.None())
    }

    /// `config.get_verbosity(type)`: the level for a fine-grained verbosity
    /// type, or the global verbose level when the type is unknown/unset.
    #[pyo3(signature = (verbosity_type = None))]
    fn get_verbosity(
        &self,
        py: Python<'_>,
        verbosity_type: Option<String>,
    ) -> PyResult<Py<PyAny>> {
        let global_level = self.option.bind(py).getattr("verbose")?.unbind();
        let Some(vt) = verbosity_type else {
            return Ok(global_level);
        };
        let ini_name = format!("verbosity_{vt}");
        // The verbosity ini must be registered (a plugin's addini); otherwise
        // fall back to the global level (upstream's `_parser._inidict` check).
        let registered = py
            .import("pytest._parser")?
            .getattr("ini_specs")?
            .contains(ini_name.as_str())?;
        if !registered {
            return Ok(global_level);
        }
        let level = self.getini(py, &ini_name)?;
        if level.bind(py).extract::<String>().ok().as_deref() == Some("auto") {
            return Ok(global_level);
        }
        Ok(py
            .import("builtins")?
            .getattr("int")?
            .call1((level.bind(py),))?
            .unbind())
    }

    /// xdist parity: present (with the worker id) only in -n workers, so
    /// `hasattr(config, "workerinput")` detects worker processes. Custom
    /// entries set by controller-side pytest_configure_node hooks arrive
    /// as JSON via PYTEST_RS_WORKERINPUT.
    #[getter]
    fn workerinput<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        match std::env::var("PYTEST_XDIST_WORKER") {
            Ok(worker_id) => {
                let dict = pyo3::types::PyDict::new(py);
                dict.set_item("workerid", worker_id)?;
                if let Ok(count) = std::env::var("PYTEST_XDIST_WORKER_COUNT") {
                    dict.set_item("workercount", count.parse::<usize>().unwrap_or(1))?;
                }
                if let Ok(uid) = std::env::var("PYTEST_XDIST_TESTRUNUID") {
                    dict.set_item("testrunuid", uid)?;
                }
                if let Ok(json) = std::env::var("PYTEST_RS_WORKERINPUT")
                    && let Ok(extra) = py.import("json")?.call_method1("loads", (json,))
                {
                    dict.call_method1("update", (extra,))?;
                }
                Ok(dict.into_any())
            }
            Err(_) => Err(pyo3::exceptions::PyAttributeError::new_err("workerinput")),
        }
    }

    /// xdist parity: a worker-only dict the controller receives back in
    /// pytest_testnodedown (node.workeroutput).
    #[getter]
    fn workeroutput<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        if std::env::var("PYTEST_XDIST_WORKER").is_err() {
            return Err(pyo3::exceptions::PyAttributeError::new_err("workeroutput"));
        }
        py.import("pytest._dist")?.getattr("workeroutput")
    }

    #[getter]
    fn rootpath<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        py.import("pathlib")?
            .getattr("Path")?
            .call1((self.rootdir.as_str(),))
    }

    #[getter]
    fn rootdir<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        self.rootpath(py)
    }

    /// `config.inipath`: a Path to the discovered config file, or None.
    #[getter]
    fn inipath<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        match &self.inipath {
            Some(path) => py.import("pathlib")?.getattr("Path")?.call1((path.as_str(),)),
            None => Ok(py.None().into_bound(py)),
        }
    }

    /// `config.inifile`: legacy py.path alias for inipath (deprecated form).
    #[getter]
    fn inifile<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        match &self.inipath {
            Some(path) => py
                .import("pytest._tmp_path")?
                .getattr("LocalPath")?
                .call1((path.as_str(),)),
            None => Ok(py.None().into_bound(py)),
        }
    }

    #[getter]
    fn args(&self, py: Python<'_>) -> Py<PyAny> {
        pyo3::types::PyList::new(py, &self.args)
            .map(|list| list.into_any().unbind())
            .unwrap_or_else(|_| py.None())
    }

    /// `config.args_source`: the upstream `Config.ArgsSource` enum member
    /// (ARGS / TESTPATHS / INVOCATION_DIR) matching how `args` was derived.
    #[getter]
    fn args_source<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let member = match self.args_source.as_str() {
            "args" => "ARGS",
            "testpaths" => "TESTPATHS",
            _ => "INVOCATION_DIR",
        };
        py.import("_pytest.config")?
            .getattr("Config")?
            .getattr("ArgsSource")?
            .getattr(member)
    }

    /// pytest's parseconfigure step: fire pytest_configure. Kept minimal —
    /// the cache/getini/getoption surface parseconfig tests exercise needs
    /// no plugin configuration, and re-firing the shared pluginmanager's
    /// hooks would reconfigure the outer session's plugins.
    fn _do_configure(&self) {}

    /// pytest's config teardown (registered as a parseconfig finalizer).
    fn _ensure_unconfigure(&self) {}

    /// Minimal pluginmanager (getplugin("logging-plugin") etc.).
    #[getter]
    fn pluginmanager<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        py.import("pytest._pluginmanager")?.getattr("pluginmanager")
    }

    /// config.hook: the pluggy-lite hook relay (config.hook.<name>(**kw)
    /// dispatches to every registered plugin, e.g. sugar's header calling
    /// config.hook.pytest_report_header).
    #[getter]
    fn hook<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        self.pluginmanager(py)?.getattr("hook")
    }

    /// A TerminalWriter on the ORIGINAL stdout (sys.__stdout__, fd 1) —
    /// upstream's is created before capture replaces sys.stdout, so
    /// out-of-band dumps (pytest-timeout's stack dump before os._exit)
    /// reach the real terminal once capture is suspended.
    fn get_terminal_writer<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let stdout = py.import("sys")?.getattr("__stdout__")?;
        let file = if stdout.is_none() {
            py.None().into_bound(py)
        } else {
            stdout
        };
        py.import("_pytest._io")?
            .getattr("TerminalWriter")?
            .call1((file,))
    }

    /// The pytest `config.cache` API (a pytest._cache.Cache).
    #[getter]
    fn cache<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let cls = py.import("pytest._cache")?.getattr("Cache")?;
        let dir = cls.call_method1(
            "cache_dir_from",
            (
                self.rootdir.as_str(),
                self.ini
                    .lock()
                    .expect("config lock poisoned")
                    .get("cache_dir")
                    .cloned()
                    .unwrap_or_default(),
            ),
        )?;
        cls.call1((dir,))
    }
}

/// A (subset of the) pytest `FixtureRequest` API. Constructed per fixture
/// setup; finalizers registered through it are drained into the session's
/// finalizer stack by the resolver afterwards.
#[pyclass(name = "FixtureRequest")]
pub struct PyRequest {
    param: Option<Py<PyAny>>,
    node: Py<PyAny>,
    fixturename: Option<String>,
    finalizers: Mutex<Vec<Py<PyAny>>>,
}

impl PyRequest {
    pub fn new(param: Option<Py<PyAny>>, node: Py<PyAny>, fixturename: Option<String>) -> Self {
        Self {
            param,
            node,
            fixturename,
            finalizers: Mutex::new(Vec::new()),
        }
    }

    /// Finalizers registered via addfinalizer, in registration order.
    pub fn take_finalizers(&self) -> Vec<Py<PyAny>> {
        std::mem::take(&mut self.finalizers.lock().expect("request lock poisoned"))
    }
}

#[pymethods]
impl PyRequest {
    /// Engine use: run (and clear) the addfinalizer callbacks, LIFO. Called
    /// at the owning fixture's teardown so finalizers added late (factory
    /// fixtures calling addfinalizer during the test) are included.
    fn _drain_finalizers(&self, py: Python<'_>) -> PyResult<()> {
        let mut first_err: Option<PyErr> = None;
        for finalizer in self.take_finalizers().into_iter().rev() {
            if let Err(err) = finalizer.bind(py).call0()
                && first_err.is_none()
            {
                first_err = Some(err);
            }
        }
        match first_err {
            Some(err) => Err(err),
            None => Ok(()),
        }
    }

    /// The current parameter for parametrized fixtures. AttributeError when
    /// the fixture is not parametrized, matching pytest.
    #[getter]
    fn param(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        match &self.param {
            Some(value) => Ok(value.clone_ref(py)),
            None => Err(pyo3::exceptions::PyAttributeError::new_err("param")),
        }
    }

    #[getter]
    fn fixturename(&self) -> Option<String> {
        self.fixturename.clone()
    }

    /// The `request.node` object (a pytest._node.Node shim instance).
    #[getter]
    fn node(&self, py: Python<'_>) -> Py<PyAny> {
        self.node.clone_ref(py)
    }

    /// Names of all fixtures visible to this request's item.
    #[getter]
    fn fixturenames(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        Ok(self.node.bind(py).getattr("fixturenames")?.unbind())
    }

    /// The item's keywords mapping (pytest's request.keywords == node.keywords).
    #[getter]
    fn keywords(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        Ok(self.node.bind(py).getattr("keywords")?.unbind())
    }

    /// The underlying test function object.
    /// Returns None for doctest items (they have no user-visible test function).
    #[getter]
    fn function(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        let node = self.node.bind(py);
        if node
            .getattr("_pytest_doctest_item")
            .ok()
            .and_then(|v| v.extract::<bool>().ok())
            .unwrap_or(false)
        {
            return Ok(py.None());
        }
        Ok(node.getattr("function")?.unbind())
    }

    /// The class containing the test, or None for functions/doctests.
    #[getter]
    fn cls(&self, py: Python<'_>) -> Py<PyAny> {
        crate::runner::current_resolve_instance(py)
            .map(|instance| instance.bind(py).get_type().unbind().into_any())
            .unwrap_or_else(|| py.None())
    }

    /// The test instance the item runs on (a fresh Test class instance, or
    /// the unittest.TestCase instance), or None for plain functions.
    #[getter]
    fn instance(&self, py: Python<'_>) -> Py<PyAny> {
        crate::runner::current_resolve_instance(py).unwrap_or_else(|| py.None())
    }

    /// The module containing the test, or None for doctests.
    #[getter]
    fn module(&self, py: Python<'_>) -> Py<PyAny> {
        py.None()
    }

    /// Dynamically resolve (and cache) a fixture by name, like pytest's
    /// request.getfixturevalue. Delegates to the runner's per-item context.
    fn getfixturevalue(&self, py: Python<'_>, argname: &str) -> PyResult<Py<PyAny>> {
        crate::runner::getfixturevalue(py, argname)
    }

    /// Apply a marker to the running item (pytest's request.applymarker);
    /// the engine re-evaluates xfail against dynamically added marks.
    fn applymarker(&self, py: Python<'_>, marker: Py<PyAny>) -> PyResult<()> {
        self.node.bind(py).call_method1("add_marker", (marker,))?;
        Ok(())
    }

    fn addfinalizer(&self, finalizer: Py<PyAny>) {
        self.finalizers
            .lock()
            .expect("request lock poisoned")
            .push(finalizer);
    }

    /// The session config proxy (pytest's `request.config`).
    #[getter]
    fn config(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        crate::python::existing_py_config(py)
            .ok_or_else(|| pyo3::exceptions::PyAttributeError::new_err("config"))
    }
}
