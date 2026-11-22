//! The record-time collector (docs/affected.md): a global
//! sys.monitoring `PY_START` callback that notes which project code objects
//! run under the current context. Each code object is classified once
//! (project file or not) and pinned so its id is never reused; the callback
//! returns DISABLE, and every context switch calls `restart_events()` so
//! each function fires once more under the new context.
//!
//! In record runs an audit hook adds the non-Python inputs of the
//! current context: files opened for reading and directories listed under
//! the rootdir (hashed at first sight), and whether a subprocess was
//! started (its dependencies are unknown).
//!
//! Keeping `PY_START` armed and deduplicating in Rust instead was measured
//! on public suites and lost: it costs one callback per call, which
//! call-heavy suites pay heavily for.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use pyo3::prelude::*;
use pyo3::types::PyTuple;

use crate::units::FileUnits;

/// A non-Python input recorded by the audit hook.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum InputKind {
    /// A file opened for reading: its content hash.
    Content,
    /// A directory listed: the hash of its sorted entry names.
    Listing,
}

impl InputKind {
    /// The pseudo qualname a dependency of this kind is stored under.
    pub fn qualname(self) -> &'static str {
        match self {
            InputKind::Content => "<content>",
            InputKind::Listing => "<listing>",
        }
    }

    pub fn from_qualname(qualname: &str) -> Option<Self> {
        match qualname {
            "<content>" => Some(InputKind::Content),
            "<listing>" => Some(InputKind::Listing),
            _ => None,
        }
    }

    /// Directory entries left out of a listing hash: what pytest-rs and
    /// Python write next to the code on every run (the default data file
    /// and its temp files, the default cache directory, bytecode caches).
    fn own_entry(name: &str) -> bool {
        name.starts_with(".pytest-rs-affected")
            || name == ".pytest_cache"
            || name == "__pycache__"
            || name.ends_with(".pyc")
    }

    /// The current hash of `path` for this kind; 0 when it is missing or
    /// unreadable.
    pub fn hash(self, path: &Path) -> u64 {
        match self {
            InputKind::Content => std::fs::read(path)
                .map(|bytes| FileUnits::content_hash(&bytes))
                .unwrap_or(0),
            InputKind::Listing => match std::fs::read_dir(path) {
                Ok(entries) => {
                    let mut names: Vec<String> = entries
                        .filter_map(Result::ok)
                        .map(|e| e.file_name().to_string_lossy().into_owned())
                        .filter(|name| !Self::own_entry(name))
                        .collect();
                    names.sort();
                    FileUnits::content_hash(names.join("\0").as_bytes())
                }
                Err(_) => 0,
            },
        }
    }
}

/// What one session recorded: the project files seen (with their content
/// at first sight, so an edit during the run cannot be mistaken for the
/// code that ran), the code objects (file index, `co_qualname`), and the
/// code objects run under each context.
pub struct Recording {
    pub files: Vec<(String, Option<Vec<u8>>)>,
    pub codes: Vec<(u32, String)>,
    pub contexts: Vec<HashSet<u32>>,
    /// Non-Python inputs: (absolute path, kind, hash at first sight).
    pub inputs: Vec<(String, InputKind, u64)>,
    /// The inputs read under each context.
    pub context_inputs: Vec<HashSet<u32>>,
    /// Contexts that started a subprocess.
    pub incomplete: HashSet<usize>,
    /// The import context of each project module executed through the
    /// import system: absolute file -> context.
    pub import_contexts: HashMap<String, usize>,
    /// (enclosing context, import context) for each module executed: the
    /// module's import ran while the enclosing context was current.
    pub nested: Vec<(usize, usize)>,
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
    inputs: Vec<(String, InputKind, u64)>,
    input_index: HashMap<(String, InputKind), u32>,
    context_inputs: Vec<HashSet<u32>>,
    incomplete: HashSet<usize>,
    /// Modules being executed, innermost last: while one runs, its import
    /// context is current instead of `current`.
    import_stack: Vec<usize>,
    import_contexts: HashMap<String, usize>,
    nested: Vec<(usize, usize)>,
}

impl State {
    /// The context code runs under right now.
    fn effective(&self) -> usize {
        self.import_stack.last().copied().unwrap_or(self.current)
    }

    fn new_context(&mut self) -> usize {
        self.contexts.push(HashSet::new());
        self.context_inputs.push(HashSet::new());
        self.contexts.len() - 1
    }
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
    /// Files and directories the audit hook never records, absolute (the
    /// data file, the cache directory).
    ignored_inputs: Vec<String>,
    /// Cleared at the end of the session: an audit hook cannot be removed.
    auditing: AtomicBool,
    /// The embedded shim directory (with a trailing separator).
    shim_root: Option<String>,
    state: Mutex<State>,
}

impl AffectedCollector {
    pub fn new(
        rootdir: String,
        excluded: Vec<String>,
        disable: Py<PyAny>,
        monitoring: Py<PyAny>,
        ignored_inputs: Vec<String>,
        shim_root: Option<String>,
    ) -> Self {
        Self {
            rootdir,
            excluded,
            disable,
            monitoring,
            ignored_inputs,
            auditing: AtomicBool::new(true),
            shim_root,
            state: Mutex::new(State {
                contexts: vec![HashSet::new()],
                context_inputs: vec![HashSet::new()],
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
        self.lock().new_context()
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
        self.monitoring.bind(py).call_method0("restart_events")?;
        Ok(())
    }

    /// Stop recording (the audit hook stays installed but inert) and hand
    /// over what was recorded.
    pub fn take(&self) -> Recording {
        self.auditing.store(false, Ordering::Relaxed);
        let mut state = self.lock();
        Recording {
            files: std::mem::take(&mut state.files),
            codes: std::mem::take(&mut state.codes),
            contexts: std::mem::take(&mut state.contexts),
            inputs: std::mem::take(&mut state.inputs),
            context_inputs: std::mem::take(&mut state.context_inputs),
            incomplete: std::mem::take(&mut state.incomplete),
            import_contexts: std::mem::take(&mut state.import_contexts),
            nested: std::mem::take(&mut state.nested),
        }
    }

    /// A non-Python input under the rootdir worth recording.
    fn input_path(&self, path: PathBuf) -> Option<String> {
        let path = if path.is_absolute() {
            path
        } else {
            std::env::current_dir().ok()?.join(path)
        };
        let path = path.to_string_lossy().into_owned();
        let tracked = path.starts_with(&self.rootdir)
            && !self
                .excluded
                .iter()
                .any(|dir| path.starts_with(dir.as_str()))
            && !self
                .ignored_inputs
                .iter()
                .any(|ignored| path == *ignored || path.starts_with(&format!("{ignored}/")))
            && !path
                .split(['/', '\\'])
                .any(|part| part == "__pycache__" || part == "site-packages")
            && ![".py", ".pyc", ".pyo", ".so", ".pyd"]
                .iter()
                .any(|suffix| path.ends_with(suffix));
        tracked.then_some(path)
    }

    fn record_input(&self, path: String, kind: InputKind) {
        let mut state = self.lock();
        let index = match state.input_index.get(&(path.clone(), kind)) {
            Some(&index) => index,
            None => {
                let hash = kind.hash(Path::new(&path));
                state.inputs.push((path.clone(), kind, hash));
                let index = (state.inputs.len() - 1) as u32;
                state.input_index.insert((path, kind), index);
                index
            }
        };
        let current = state.effective();
        state.context_inputs[current].insert(index);
    }

    /// Whether the Python code that raised the audit event is the import
    /// system or pytest-rs's own shim: their reads and listings (finding
    /// modules on sys.path, loading conftests) are not test inputs.
    fn raised_by_machinery(&self, py: Python<'_>) -> bool {
        let filename: Option<String> = py
            .import("sys")
            .and_then(|sys| sys.call_method1("_getframe", (0,)))
            .and_then(|frame| frame.getattr("f_code"))
            .and_then(|code| code.getattr("co_filename"))
            .and_then(|f| f.extract())
            .ok();
        filename.is_some_and(|f| {
            f.starts_with("<frozen importlib")
                || f.starts_with("<frozen zipimport")
                || f.contains("importlib/_bootstrap")
                || self
                    .shim_root
                    .as_deref()
                    .is_some_and(|shim| f.starts_with(shim))
        })
    }

    /// Whether an `open` audit event opens for reading only.
    fn opens_for_reading(args: &Bound<'_, PyTuple>) -> bool {
        let mode = args.get_item(1).ok();
        match mode.as_ref().and_then(|m| m.extract::<String>().ok()) {
            Some(mode) => !mode.contains(['w', 'a', 'x', '+']),
            None => args
                .get_item(2)
                .ok()
                .and_then(|flags| flags.extract::<i64>().ok())
                .is_none_or(|flags| flags & 0b11 == 0),
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
    /// Whether the import system should bracket this module's execution
    /// (`spec.origin`): a project Python file.
    fn tracks_module(&self, origin: &str) -> bool {
        self.tracked(origin)
    }

    /// A project module starts executing (its loader's `exec_module`): its
    /// own import context becomes current. Returns the stack depth to pass
    /// to `import_finish`.
    fn import_start(&self, py: Python<'_>, origin: &str) -> PyResult<usize> {
        let depth = {
            let mut state = self.lock();
            let enclosing = state.effective();
            let context = match state.import_contexts.get(origin) {
                Some(&context) => context,
                None => {
                    let context = state.new_context();
                    state.import_contexts.insert(origin.to_string(), context);
                    context
                }
            };
            state.nested.push((enclosing, context));
            state.import_stack.push(context);
            state.import_stack.len()
        };
        self.monitoring.bind(py).call_method0("restart_events")?;
        Ok(depth)
    }

    /// The module started at `depth` finished (or raised): back to the
    /// context that was current before it.
    fn import_finish(&self, py: Python<'_>, depth: usize) -> PyResult<()> {
        self.lock().import_stack.truncate(depth.saturating_sub(1));
        self.monitoring.bind(py).call_method0("restart_events")?;
        Ok(())
    }

    /// The `sys.addaudithook` callback. Filters on the event name before
    /// doing any work.
    fn audit(&self, py: Python<'_>, event: &str, args: Bound<'_, PyTuple>) {
        if !self.auditing.load(Ordering::Relaxed) {
            return;
        }
        let input_event = matches!(event, "open" | "os.listdir" | "os.scandir");
        if input_event && self.raised_by_machinery(py) {
            return;
        }
        match event {
            "open" => {
                if !Self::opens_for_reading(&args) {
                    return;
                }
                let Some(path) = args
                    .get_item(0)
                    .ok()
                    .and_then(|p| p.extract::<PathBuf>().ok())
                    .and_then(|p| self.input_path(p))
                else {
                    return;
                };
                if !Path::new(&path).is_dir() {
                    self.record_input(path, InputKind::Content);
                }
            }
            "os.listdir" | "os.scandir" => {
                let path = args
                    .get_item(0)
                    .ok()
                    .filter(|p| !p.is_none())
                    .and_then(|p| p.extract::<PathBuf>().ok())
                    .unwrap_or_else(|| PathBuf::from("."));
                if let Some(path) = self.input_path(path) {
                    self.record_input(path, InputKind::Listing);
                }
            }
            "subprocess.Popen" | "os.posix_spawn" | "os.exec" | "os.spawn" | "os.system"
            | "os.fork" | "os.forkpty" | "os.startfile" => {
                let mut state = self.lock();
                let current = state.effective();
                state.incomplete.insert(current);
            }
            _ => {}
        }
    }

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
                    let current = state.effective();
                    state.contexts[current].insert(index);
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
            let current = state.effective();
            state.contexts[current].insert(index);
            index
        });
        state.classified.insert(key, index);
        state.pins.push(code.unbind());
        Ok(self.disable.clone_ref(py))
    }
}
