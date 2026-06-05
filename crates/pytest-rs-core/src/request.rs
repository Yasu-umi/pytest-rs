//! The Python-visible `request` fixture and `config` proxy objects.

use std::collections::HashMap;
use std::sync::Mutex;

use pyo3::prelude::*;

/// A (subset of the) pytest `Config` API passed to conftest hooks.
#[pyclass(name = "Config")]
pub struct PyConfig {
    rootdir: String,
    ini: Mutex<HashMap<String, String>>,
    /// The argparse-namespace equivalent (`config.option`), mutable from
    /// Python so conftest hooks can stash flags on it.
    #[pyo3(get)]
    option: Py<PyAny>,
}

impl PyConfig {
    pub fn new(rootdir: String, ini: HashMap<String, String>, option: Py<PyAny>) -> Self {
        Self {
            rootdir,
            ini: Mutex::new(ini),
            option,
        }
    }
}

#[pymethods]
impl PyConfig {
    fn getini(&self, name: &str) -> Option<String> {
        self.ini
            .lock()
            .expect("config lock poisoned")
            .get(name)
            .cloned()
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
        match ns.getattr(attr.as_str()) {
            Ok(v) => Ok(v.into()),
            Err(_) => Ok(default.unwrap_or_else(|| py.None())),
        }
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

    /// xdist parity: present (with the worker id) only in -n workers, so
    /// `hasattr(config, "workerinput")` detects worker processes.
    #[getter]
    fn workerinput<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        match std::env::var("PYTEST_XDIST_WORKER") {
            Ok(worker_id) => {
                let dict = pyo3::types::PyDict::new(py);
                dict.set_item("workerid", worker_id)?;
                Ok(dict.into_any())
            }
            Err(_) => Err(pyo3::exceptions::PyAttributeError::new_err("workerinput")),
        }
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

    /// Minimal pluginmanager (getplugin("logging-plugin") etc.).
    #[getter]
    fn pluginmanager<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        py.import("pytest._pluginmanager")?.getattr("pluginmanager")
    }

    /// The pytest `config.cache` API (a pytest._cache.Cache).
    #[getter]
    fn cache<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let cls = py.import("pytest._cache")?.getattr("Cache")?;
        let dir = cls.call_method1(
            "cache_dir_from",
            (
                self.rootdir.as_str(),
                self.getini("cache_dir").unwrap_or_default(),
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

    /// The underlying test function object.
    #[getter]
    fn function(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        Ok(self.node.bind(py).getattr("function")?.unbind())
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
