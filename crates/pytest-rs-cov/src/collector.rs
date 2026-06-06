//! The sys.monitoring callbacks, coverage.py sysmon-style two stages: a
//! global PY_START gate classifies each code object exactly once, arming
//! local LINE events only on tracked code. Untracked code (site-packages,
//! stdlib, the shim) is never line-instrumented, so the per-process
//! first-hit cost — paid again by every fork worker — shrinks from "every
//! line of every dependency" to "one PY_START per code object".

use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, Mutex};

use pyo3::prelude::*;

/// id(code) -> (pinned code object, filename) for tracked code.
type TrackedCodes = Mutex<HashMap<usize, (Py<PyAny>, Arc<str>)>>;

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
    /// The pin keeps each tracked code object alive so its id is never
    /// reused; the cached filename spares the LINE callback per-line
    /// getattrs.
    tracked_codes: TrackedCodes,
    disable: Py<PyAny>,
    /// sys.monitoring.events.LINE, for set_local_events.
    line_event: Py<PyAny>,
    monitoring: Py<PyAny>,
    tool_id: u8,
}

impl LineCollector {
    pub fn new(
        rootdir: String,
        sources: Vec<String>,
        shim_root: String,
        disable: Py<PyAny>,
        line_event: Py<PyAny>,
        monitoring: Py<PyAny>,
        tool_id: u8,
    ) -> Self {
        Self {
            rootdir,
            sources,
            shim_root,
            hits: Mutex::new(HashMap::new()),
            tracked_codes: Mutex::new(HashMap::new()),
            disable,
            line_event,
            monitoring,
            tool_id,
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
    /// Global PY_START gate: classify the code object once. Tracked code
    /// gets local LINE events (effective immediately, before this frame's
    /// first line — coverage.py relies on the same ordering); everything
    /// returns DISABLE, so each code object pays one PY_START ever.
    fn py_start(
        &self,
        py: Python<'_>,
        code: Bound<'_, PyAny>,
        _offset: i64,
    ) -> PyResult<Py<PyAny>> {
        let key = code.as_ptr() as usize;
        let already_tracked = self
            .tracked_codes
            .lock()
            .expect("collector lock poisoned")
            .contains_key(&key);
        if !already_tracked {
            let filename: String = code.getattr("co_filename")?.extract()?;
            // Like coverage.py: synthesized annotation scopes are not
            // user code.
            let name: String = code.getattr("co_name")?.extract()?;
            if name != "__annotate__" && self.tracked(&filename) {
                self.monitoring.bind(py).call_method1(
                    "set_local_events",
                    (self.tool_id, &code, self.line_event.bind(py)),
                )?;
                self.tracked_codes
                    .lock()
                    .expect("collector lock poisoned")
                    .insert(key, (code.unbind(), Arc::from(filename)));
            }
        }
        Ok(self.disable.clone_ref(py))
    }

    /// Local LINE on tracked code: record the hit and DISABLE, so every
    /// tracked line costs exactly one callback ever.
    fn line(&self, py: Python<'_>, code: Bound<'_, PyAny>, line: u32) -> PyResult<Py<PyAny>> {
        let key = code.as_ptr() as usize;
        let filename = self
            .tracked_codes
            .lock()
            .expect("collector lock poisoned")
            .get(&key)
            .map(|(_, filename)| Arc::clone(filename));
        if let Some(filename) = filename {
            self.hits
                .lock()
                .expect("collector lock poisoned")
                .entry(filename.as_ref().to_string())
                .or_default()
                .insert(line);
        }
        Ok(self.disable.clone_ref(py))
    }
}
