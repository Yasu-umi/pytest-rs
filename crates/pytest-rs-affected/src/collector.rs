//! The record-time collector (docs/AFFECTED_TESTS.md §4.2): a global
//! sys.monitoring `PY_START` callback that notes which project code objects
//! run under the current context. Each code object is classified once
//! (project file or not) and pinned so its id is never reused; the callback
//! returns DISABLE, and every context switch calls `restart_events()` so
//! each function fires once more under the new context.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use pyo3::prelude::*;

/// What one session recorded: the project files seen (with their content
/// at first sight, so an edit during the run cannot be mistaken for the
/// code that ran), the code objects (file index, `co_qualname`), and the
/// code objects run under each context.
pub struct Recording {
    pub files: Vec<(String, Option<Vec<u8>>)>,
    pub codes: Vec<(u32, String)>,
    pub contexts: Vec<HashSet<u32>>,
}

#[derive(Default)]
struct State {
    /// Code object address -> code index (None = not project code).
    classified: HashMap<usize, Option<u32>>,
    /// Keeps every classified code object alive, so no address is reused.
    pins: Vec<Py<PyAny>>,
    codes: Vec<(u32, String)>,
    files: Vec<(String, Option<Vec<u8>>)>,
    file_index: HashMap<String, u32>,
    contexts: Vec<HashSet<u32>>,
    current: usize,
}

#[pyclass]
pub struct AffectedCollector {
    /// rootdir with a trailing separator.
    rootdir: String,
    /// Directories never recorded (stdlib, site-packages, the embedded
    /// shim), each with a trailing separator.
    excluded: Vec<String>,
    disable: Py<PyAny>,
    monitoring: Py<PyAny>,
    /// MEASUREMENT ONLY: keep project code's PY_START armed and dedupe per
    /// context instead of DISABLE + restart_events.
    stay_armed: bool,
    state: Mutex<State>,
}

impl AffectedCollector {
    pub fn new(
        rootdir: String,
        excluded: Vec<String>,
        disable: Py<PyAny>,
        monitoring: Py<PyAny>,
    ) -> Self {
        Self {
            rootdir,
            excluded,
            disable,
            monitoring,
            stay_armed: std::env::var_os("PYTEST_RS_AFFECTED_STAY_ARMED").is_some(),
            state: Mutex::new(State {
                contexts: vec![HashSet::new()],
                ..State::default()
            }),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().expect("affected collector lock poisoned")
    }

    /// A fresh, empty context; its index identifies it from then on.
    /// Context 0 is the global one.
    pub fn new_context(&self) -> usize {
        let mut state = self.lock();
        state.contexts.push(HashSet::new());
        state.contexts.len() - 1
    }

    /// Make `context` current. Re-arms the DISABLEd `PY_START` events so
    /// every function fires once more under it.
    pub fn switch(&self, py: Python<'_>, context: usize) -> PyResult<()> {
        {
            let mut state = self.lock();
            if state.current == context {
                return Ok(());
            }
            state.current = context;
        }
        if !self.stay_armed {
            self.monitoring.bind(py).call_method0("restart_events")?;
        }
        Ok(())
    }

    pub fn take(&self) -> Recording {
        let mut state = self.lock();
        Recording {
            files: std::mem::take(&mut state.files),
            codes: std::mem::take(&mut state.codes),
            contexts: std::mem::take(&mut state.contexts),
        }
    }

    fn tracked(&self, filename: &str) -> bool {
        filename.ends_with(".py")
            && filename.starts_with(&self.rootdir)
            && !self
                .excluded
                .iter()
                .any(|dir| filename.starts_with(dir.as_str()))
            && !filename
                .split(['/', '\\'])
                .any(|part| part == "site-packages" || part == "dist-packages")
    }
}

#[pymethods]
impl AffectedCollector {
    fn py_start(
        &self,
        py: Python<'_>,
        code: Bound<'_, PyAny>,
        _offset: i64,
    ) -> PyResult<Py<PyAny>> {
        let key = code.as_ptr() as usize;
        {
            let mut state = self.lock();
            if let Some(entry) = state.classified.get(&key).copied() {
                if let Some(index) = entry {
                    let current = state.current;
                    state.contexts[current].insert(index);
                    if self.stay_armed {
                        return Ok(py.None());
                    }
                }
                return Ok(self.disable.clone_ref(py));
            }
        }
        let filename: String = code.getattr("co_filename")?.extract()?;
        let entry = if self.tracked(&filename) {
            let qualname: String = code.getattr("co_qualname")?.extract()?;
            Some(qualname)
        } else {
            None
        };
        let mut state = self.lock();
        let index = entry.map(|qualname| {
            let file = match state.file_index.get(&filename) {
                Some(&file) => file,
                None => {
                    let file = state.files.len() as u32;
                    let content = std::fs::read(&filename).ok();
                    state.files.push((filename.clone(), content));
                    state.file_index.insert(filename.clone(), file);
                    file
                }
            };
            state.codes.push((file, qualname));
            let index = (state.codes.len() - 1) as u32;
            let current = state.current;
            state.contexts[current].insert(index);
            index
        });
        state.classified.insert(key, index);
        state.pins.push(code.unbind());
        if self.stay_armed && index.is_some() {
            return Ok(py.None());
        }
        Ok(self.disable.clone_ref(py))
    }
}
