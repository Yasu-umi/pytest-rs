//! The sys.monitoring LINE callback: a Rust callable that records the hit
//! and returns DISABLE, so every line costs exactly one callback ever.

use std::collections::{BTreeSet, HashMap};
use std::sync::Mutex;

use pyo3::prelude::*;

#[pyclass]
pub struct LineCollector {
    /// rootdir with a trailing separator, for prefix matching.
    rootdir: String,
    /// Source filters (absolute, dirs end with a separator); empty = any
    /// file under rootdir.
    sources: Vec<String>,
    /// The embedded shim dir; never measured.
    shim_root: String,
    hits: Mutex<HashMap<String, BTreeSet<u32>>>,
    disable: Py<PyAny>,
}

impl LineCollector {
    pub fn new(
        rootdir: String,
        sources: Vec<String>,
        shim_root: String,
        disable: Py<PyAny>,
    ) -> Self {
        Self {
            rootdir,
            sources,
            shim_root,
            hits: Mutex::new(HashMap::new()),
            disable,
        }
    }

    pub fn take_hits(&self) -> HashMap<String, BTreeSet<u32>> {
        std::mem::take(&mut self.hits.lock().expect("collector lock poisoned"))
    }

    fn tracked(&self, filename: &str) -> bool {
        if filename.starts_with('<')
            || filename.starts_with(&self.shim_root)
            || filename.contains("site-packages")
            || filename.contains("/lib/python")
        {
            return false;
        }
        if self.sources.is_empty() {
            filename.starts_with(&self.rootdir)
        } else {
            self.sources.iter().any(|source| {
                filename == source.trim_end_matches('/') || filename.starts_with(source.as_str())
            })
        }
    }
}

#[pymethods]
impl LineCollector {
    fn __call__(&self, py: Python<'_>, code: Bound<'_, PyAny>, line: u32) -> PyResult<Py<PyAny>> {
        let filename: String = code.getattr("co_filename")?.extract()?;
        if self.tracked(&filename) {
            self.hits
                .lock()
                .expect("collector lock poisoned")
                .entry(filename)
                .or_default()
                .insert(line);
        }
        Ok(self.disable.clone_ref(py))
    }
}
