//! The Python-visible `request` fixture and `config` proxy objects.

use std::collections::HashMap;
use std::sync::Mutex;

use pyo3::prelude::*;

/// A (subset of the) pytest `Config` API passed to conftest hooks.
#[pyclass(name = "Config")]
pub struct PyConfig {
    rootdir: String,
    ini: HashMap<String, String>,
    /// The argparse-namespace equivalent (`config.option`), mutable from
    /// Python so conftest hooks can stash flags on it.
    #[pyo3(get)]
    option: Py<PyAny>,
}

impl PyConfig {
    pub fn new(rootdir: String, ini: HashMap<String, String>, option: Py<PyAny>) -> Self {
        Self {
            rootdir,
            ini,
            option,
        }
    }
}

#[pymethods]
impl PyConfig {
    fn getini(&self, name: &str) -> Option<String> {
        self.ini.get(name).cloned()
    }

    #[pyo3(signature = (name, default = None))]
    fn getoption(
        &self,
        py: Python<'_>,
        name: &str,
        default: Option<Py<PyAny>>,
    ) -> PyResult<Py<PyAny>> {
        let _ = name;
        Ok(default.unwrap_or_else(|| py.None()))
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

    fn addfinalizer(&self, finalizer: Py<PyAny>) {
        self.finalizers
            .lock()
            .expect("request lock poisoned")
            .push(finalizer);
    }
}
