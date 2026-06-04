//! The Python-visible `request` fixture object.

use std::sync::Mutex;

use pyo3::prelude::*;
use pyo3::types::PyDict;

/// A (subset of the) pytest `FixtureRequest` API. Constructed per fixture
/// setup; finalizers registered through it are drained into the session's
/// finalizer stack by the resolver afterwards.
#[pyclass(name = "FixtureRequest")]
pub struct PyRequest {
    param: Option<Py<PyAny>>,
    nodeid: String,
    node_name: String,
    fixturename: Option<String>,
    finalizers: Mutex<Vec<Py<PyAny>>>,
}

impl PyRequest {
    pub fn new(
        param: Option<Py<PyAny>>,
        nodeid: String,
        node_name: String,
        fixturename: Option<String>,
    ) -> Self {
        Self {
            param,
            nodeid,
            node_name,
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

    /// A minimal `request.node` (nodeid / name attributes).
    #[getter]
    fn node<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let types = py.import("types")?;
        let kwargs = PyDict::new(py);
        kwargs.set_item("nodeid", &self.nodeid)?;
        kwargs.set_item("name", &self.node_name)?;
        types
            .getattr("SimpleNamespace")?
            .call((), Some(&kwargs))
            .map(Bound::into_any)
    }

    fn addfinalizer(&self, finalizer: Py<PyAny>) {
        self.finalizers
            .lock()
            .expect("request lock poisoned")
            .push(finalizer);
    }
}
