//! The Python-visible `request` fixture object.

use std::sync::Mutex;

use pyo3::prelude::*;

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
