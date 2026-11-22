//! Affected-test selection (`--affected`, docs/affected.md): run only the
//! tests affected by code that changed since a recorded run. Test files are
//! selected before import, items after it; code is recorded with
//! sys.monitoring, and non-Python inputs and subprocesses with an audit
//! hook. Without `--affected*` the plugin is inert: no sys.monitoring tool,
//! no data file, no behavior change.

mod collector;
mod config;
mod data;
mod git;
mod imports;
mod selection;
mod snapshot;
mod units;

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use pyo3::prelude::*;
use pytest_rs_core::collect::TestItem;
use pytest_rs_core::config::{OptDef, OptionParser};
use pytest_rs_core::hooks::{HookContext, Plugin};
use pytest_rs_core::report::Outcome;
use pytest_rs_core::session::{PendingFinalizer, ScopedSetup};

use crate::collector::{AffectedCollector, InputKind, Recording};
use crate::config::{AffectedConfig, BUILTIN_FULL_RUN_ON, BUILTIN_IGNORE, Globs, Incomplete};
use crate::data::{Data, Dep, FileEntry, FileRecord, Header, LoadError, SnapEntry, TestEntry};
use crate::git::{Git, Unchanged};
use crate::imports::StaticImports;
use crate::selection::{Plan, Reason};
use crate::snapshot::Snapshot;
use crate::units::{CHECKSUM_SCHEME, FILE_UNIT, FileUnits, MODULE_UNIT};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Off,
    /// Run whatever the other args select and record it.
    Record,
    /// Select from the data; record nothing.
    Select,
    /// Select, and record the tests that ran (`--affected` alone).
    Auto,
}

impl Mode {
    fn records(self) -> bool {
        matches!(self, Mode::Record | Mode::Auto)
    }

    fn selects(self) -> bool {
        matches!(self, Mode::Select | Mode::Auto)
    }
}

/// One test's record-time bookkeeping.
struct TestRun {
    /// Rootdir-relative path of the test's file.
    path: String,
    context: usize,
    /// Contexts of the higher-scope setups the test used.
    setups: BTreeSet<usize>,
}

/// Folding the records Python subprocesses wrote (the .pth child) into the
/// session's recording.
struct ChildRecords;

impl ChildRecords {
    fn fold(recording: &mut Recording, records: &[serde_json::Value]) {
        let mut file_index: HashMap<String, u32> = recording
            .files
            .iter()
            .enumerate()
            .map(|(i, (file, _))| (file.clone(), i as u32))
            .collect();
        let mut code_index: HashMap<(u32, String), u32> = recording
            .codes
            .iter()
            .enumerate()
            .map(|(i, code)| (code.clone(), i as u32))
            .collect();
        let mut input_index: HashMap<(String, InputKind), u32> = recording
            .inputs
            .iter()
            .enumerate()
            .map(|(i, (path, kind, _))| ((path.clone(), *kind), i as u32))
            .collect();
        // Per context: records that came back, the children they started in
        // turn, and whether any of them could not see everything.
        let mut came_back: HashMap<usize, usize> = HashMap::new();
        let mut started: HashMap<usize, usize> = HashMap::new();
        let mut blind: HashSet<usize> = HashSet::new();
        for record in records {
            let Some(context) = record["context"].as_u64().map(|c| c as usize) else {
                continue;
            };
            if context >= recording.contexts.len() {
                continue;
            }
            *came_back.entry(context).or_default() += 1;
            *started.entry(context).or_default() +=
                record["spawned"].as_u64().unwrap_or(0) as usize;
            if record["incomplete"].as_bool().unwrap_or(true) {
                blind.insert(context);
            }
            for code in record["codes"].as_array().into_iter().flatten() {
                let (Some(file), Some(qualname)) = (code[0].as_str(), code[1].as_str()) else {
                    continue;
                };
                let file_id = *file_index.entry(file.to_string()).or_insert_with(|| {
                    recording
                        .files
                        .push((file.to_string(), std::fs::read(file).ok()));
                    (recording.files.len() - 1) as u32
                });
                let id = *code_index
                    .entry((file_id, qualname.to_string()))
                    .or_insert_with(|| {
                        recording.codes.push((file_id, qualname.to_string()));
                        (recording.codes.len() - 1) as u32
                    });
                recording.contexts[context].insert(id);
            }
            for input in record["inputs"].as_array().into_iter().flatten() {
                let (Some(path), Some(kind)) = (input[0].as_str(), input[1].as_str()) else {
                    continue;
                };
                let kind = match kind {
                    "content" => InputKind::Content,
                    _ => InputKind::Listing,
                };
                let id = *input_index
                    .entry((path.to_string(), kind))
                    .or_insert_with(|| {
                        let hash = kind.hash(Path::new(path));
                        recording.inputs.push((path.to_string(), kind, hash));
                        (recording.inputs.len() - 1) as u32
                    });
                if let Some(inputs) = recording.context_inputs.get_mut(context) {
                    inputs.insert(id);
                }
            }
        }
        // Every Python child (and every Python child of theirs) must have
        // come back with a complete record; otherwise the context stays
        // incomplete.
        for (context, spawned) in &recording.spawns {
            let expected = spawned + started.get(context).copied().unwrap_or(0);
            if came_back.get(context).copied().unwrap_or(0) != expected || blind.contains(context) {
                recording.incomplete.insert(*context);
            }
        }
        for context in blind {
            recording.incomplete.insert(context);
        }
    }
}

/// How module executions nest, from one session's recording: which import
/// contexts ran while each context was current, and which file each import
/// context belongs to.
struct ImportGraph {
    children: HashMap<usize, Vec<usize>>,
    file_of: HashMap<usize, String>,
    incomplete: HashSet<usize>,
    memo: std::cell::RefCell<HashMap<usize, (HashSet<Dep>, bool)>>,
}

impl ImportGraph {
    fn new(recording: &Recording) -> Self {
        let mut children: HashMap<usize, Vec<usize>> = HashMap::new();
        for (parent, child) in &recording.nested {
            if parent != child {
                children.entry(*parent).or_default().push(*child);
            }
        }
        Self {
            children,
            file_of: recording
                .import_contexts
                .iter()
                .map(|(file, context)| (*context, file.clone()))
                .collect(),
            incomplete: recording.incomplete.clone(),
            memo: std::cell::RefCell::new(HashMap::new()),
        }
    }

    fn children(&self, context: usize) -> Vec<usize> {
        self.children.get(&context).cloned().unwrap_or_default()
    }

    /// A context's own dependencies plus those of every module executed
    /// while it was current (transitively), and whether any of them
    /// started a subprocess.
    fn effective(&self, context: usize, own: &[HashSet<Dep>]) -> (HashSet<Dep>, bool) {
        self.effective_inner(context, own, &mut HashSet::new())
    }

    fn effective_inner(
        &self,
        context: usize,
        own: &[HashSet<Dep>],
        visiting: &mut HashSet<usize>,
    ) -> (HashSet<Dep>, bool) {
        if let Some(done) = self.memo.borrow().get(&context) {
            return done.clone();
        }
        if !visiting.insert(context) {
            // A module re-executed inside itself (reload): already counted.
            return (HashSet::new(), false);
        }
        let mut deps: HashSet<Dep> = own.get(context).cloned().unwrap_or_default();
        let mut incomplete = self.incomplete.contains(&context);
        for child in self.children(context) {
            let (child_deps, child_incomplete) = self.effective_inner(child, own, visiting);
            deps.extend(child_deps);
            incomplete |= child_incomplete;
        }
        visiting.remove(&context);
        self.memo
            .borrow_mut()
            .insert(context, (deps.clone(), incomplete));
        (deps, incomplete)
    }

    /// The import contexts reachable from `roots` through module executions
    /// and through each module's own import statements (`static_edges`).
    fn closure(
        &self,
        roots: Vec<usize>,
        static_edges: &dyn Fn(&str) -> Vec<usize>,
    ) -> HashSet<usize> {
        let mut reached: HashSet<usize> = HashSet::new();
        let mut stack = roots;
        while let Some(context) = stack.pop() {
            if !reached.insert(context) {
                continue;
            }
            stack.extend(self.children(context));
            if let Some(file) = self.file_of.get(&context) {
                stack.extend(static_edges(file));
            }
        }
        reached
    }
}

/// Contexts are numbered by the collector; 0 is the global one
/// (`collect:global`): plugin and conftest imports, collection hooks
/// and everything else outside a test, a higher-scope setup or a test
/// module's import.
const GLOBAL_CONTEXT: usize = 0;

#[derive(Default)]
struct State {
    mode: Option<Mode>,
    rootdir: PathBuf,
    data_path: PathBuf,
    config: AffectedConfig,
    norecursedirs: Vec<String>,
    header: Option<Header>,
    /// Stored data usable for this run (header matched).
    stored: Option<Data>,
    /// Why there is no usable data (for the header line).
    unusable: Option<String>,
    /// The non-`.py` snapshot taken at selection time.
    snapshot: Option<Vec<SnapEntry>>,
    /// Old entries must not survive into the next file (a full-run
    /// trigger changed since they were recorded).
    discard_stored: bool,

    collector: Option<Py<AffectedCollector>>,
    tool_id: u8,
    /// Context stack; empty = global.
    stack: Vec<usize>,
    current_test: Option<String>,
    runs: HashMap<String, TestRun>,
    /// Node ids of the items this session was to run (after selection).
    to_run: HashSet<String>,
    setup_contexts: HashMap<ScopedSetup, usize>,
    collect_contexts: HashMap<String, usize>,
    /// The sys.meta_path finder that brackets project modules' execution.
    import_finder: Option<Py<PyAny>>,
    /// Python subprocesses' records: this process's output directory, and
    /// the variables every child recorder reads.
    children_out: Option<PathBuf>,
    children_env: Option<Vec<(&'static str, String)>>,
    /// The selection plan, built once per session (pre-import or at
    /// modifyitems, whichever comes first).
    plan: Option<Plan>,
    /// Test files imported this session: their module names and every node
    /// id they collected (before any deselection).
    file_modules: HashMap<String, String>,
    file_nodeids: HashMap<String, Vec<String>>,
    /// `-n` worker: where this worker wrote its recording, for
    /// `pytest_worker_dump`; and whether a full-run trigger changed.
    partial_path: Option<PathBuf>,
    /// `-n` controller: the workers' recordings, with their discard flag.
    partials: Vec<(Data, bool)>,
    /// The git fast path: the repository at configure time, and
    /// which stored states git vouches for (built with the plan).
    git: Option<Git>,
    unchanged: Option<Unchanged>,
    /// When this session started: a file modified since was not the
    /// content its recorded state came from.
    started: Option<std::time::SystemTime>,
}

/// `pytest-rs --affected-merge A B ... --affected-data OUT`: merge the data
/// files that sharded runs recorded into one, keeping the newer entry of
/// each test. Runs no tests. Returns the summary line, or an error message.
pub fn merge_files(inputs: &[PathBuf], output: &Path) -> Result<String, String> {
    if inputs.is_empty() {
        return Err("--affected-merge needs the data files to merge as arguments".into());
    }
    let mut merged: Option<Data> = None;
    for input in inputs {
        let data = Data::load(input).map_err(|err| match err {
            LoadError::Missing => format!("{}: not found", input.display()),
            LoadError::OtherFormat => {
                format!("{}: written by another data format", input.display())
            }
            LoadError::Unreadable => format!("{}: not an --affected data file", input.display()),
        })?;
        match &mut merged {
            None => merged = Some(data),
            Some(into) => {
                if let Some(reason) = data.header.mismatch(&into.header) {
                    return Err(format!(
                        "{}: cannot merge with {} ({reason})",
                        input.display(),
                        inputs[0].display()
                    ));
                }
                into.merge(&data);
            }
        }
    }
    let merged = merged.expect("at least one input");
    merged
        .save(output)
        .map_err(|e| format!("cannot write {}: {e}", output.display()))?;
    Ok(format!(
        "affected: merged {} files ({} tests) into {}",
        inputs.len(),
        merged.tests.len(),
        output.display()
    ))
}

pub struct AffectedPlugin {
    state: Mutex<State>,
}

impl Default for AffectedPlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl AffectedPlugin {
    /// The ruff tag the unit checksums depend on (see the workspace
    /// Cargo.toml; a unit test keeps them in sync).
    const RUFF_TAG: &str = "0.17.5";
    const DEFAULT_DATA_FILE: &str = ".pytest-rs-affected";
    /// sys.monitoring tool ids to try. 0 is the debugger's; coverage claims
    /// 1 first and falls back to 3-5; 2 is cProfile's.
    const TOOL_IDS: [u8; 3] = [4, 3, 5];

    pub fn new() -> Self {
        Self {
            state: Mutex::new(State::default()),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().expect("affected plugin lock poisoned")
    }

    fn parse_mode(value: Option<Vec<&str>>) -> Result<Mode, String> {
        let Some(values) = value else {
            return Ok(Mode::Off);
        };
        match values.last().copied().unwrap_or("") {
            "" | "auto" => Ok(Mode::Auto),
            "record" => Ok(Mode::Record),
            "select" => Ok(Mode::Select),
            other => Err(format!(
                "argument --affected: invalid choice: '{other}' (choose from 'auto', 'record', 'select')"
            )),
        }
    }

    fn rel(rootdir: &Path, path: &Path) -> Option<String> {
        path.strip_prefix(rootdir)
            .ok()
            .map(|rel| rel.to_string_lossy().replace('\\', "/"))
    }

    fn item_path(rootdir: &Path, item: &TestItem) -> String {
        Self::rel(rootdir, &item.path).unwrap_or_else(|| {
            item.nodeid
                .split("::")
                .next()
                .unwrap_or(&item.nodeid)
                .to_string()
        })
    }

    /// The header this run would write: anything in it that differs from
    /// the stored one makes the stored data unusable.
    fn current_header(py: Python<'_>) -> PyResult<Header> {
        let sys = py.import("sys")?;
        let python = format!(
            "{} {} {} {}",
            sys.getattr("implementation")?.getattr("name")?,
            sys.getattr("version")?,
            sys.getattr("platform")?,
            py.import("platform")?.call_method0("machine")?
        );
        // Broken distribution metadata makes importlib.metadata warn; keep
        // that out of the session's warnings.
        let catch = py.import("warnings")?.call_method0("catch_warnings")?;
        catch.call_method0("__enter__")?;
        let packages = Self::installed_packages(py);
        catch.call_method1("__exit__", (py.None(), py.None(), py.None()))?;
        let packages = packages?;
        let mut hasher = FileUnits::hasher();
        std::hash::Hash::hash(&packages, &mut hasher);
        Ok(Header {
            checksum_scheme: CHECKSUM_SCHEME,
            ruff_tag: Self::RUFF_TAG.to_string(),
            pytest_rs_version: env!("CARGO_PKG_VERSION").to_string(),
            python,
            environment: std::hash::Hasher::finish(&hasher),
        })
    }

    /// Every installed distribution's (lowercased name, version), sorted.
    fn installed_packages(py: Python<'_>) -> PyResult<Vec<(String, String)>> {
        py.import("warnings")?
            .call_method1("simplefilter", ("ignore",))?;
        let mut packages: Vec<(String, String)> = Vec::new();
        for dist in py
            .import("importlib.metadata")?
            .call_method0("distributions")?
            .try_iter()?
        {
            let dist = dist?;
            let name: String = dist
                .getattr("metadata")
                .and_then(|m| m.call_method1("get", ("Name", "")))
                .and_then(|n| n.extract())
                .unwrap_or_default();
            let version: String = dist
                .getattr("version")
                .and_then(|v| v.extract())
                .unwrap_or_default();
            packages.push((name.to_lowercase(), version));
        }
        packages.sort();
        Ok(packages)
    }

    /// Directories the collector never records: the stdlib, site-packages
    /// and the embedded shim.
    fn excluded_dirs(py: Python<'_>) -> PyResult<Vec<String>> {
        let paths = py.import("sysconfig")?.call_method0("get_paths")?;
        let mut dirs: Vec<String> = ["stdlib", "platstdlib", "purelib", "platlib"]
            .iter()
            .filter_map(|key| paths.get_item(key).ok()?.extract::<String>().ok())
            .collect();
        dirs.push(
            pytest_rs_core::python::shim_root()
                .to_string_lossy()
                .into_owned(),
        );
        Ok(dirs.into_iter().map(|d| Self::with_sep(&d)).collect())
    }

    fn with_sep(path: &str) -> String {
        if path.ends_with(std::path::MAIN_SEPARATOR) {
            path.to_string()
        } else {
            format!("{path}{}", std::path::MAIN_SEPARATOR)
        }
    }

    fn start_collector(state: &mut State, py: Python<'_>, cache_dir: &str) -> PyResult<()> {
        let monitoring = py.import("sys")?.getattr("monitoring")?;
        let tool_id = Self::TOOL_IDS
            .into_iter()
            .find(|id| {
                monitoring
                    .call_method1("use_tool_id", (*id, "pytest-rs-affected"))
                    .is_ok()
            })
            .ok_or_else(|| {
                pyo3::exceptions::PyRuntimeError::new_err(
                    "no free sys.monitoring tool id for --affected",
                )
            })?;
        let collector = Py::new(
            py,
            AffectedCollector::new(
                Self::with_sep(&state.rootdir.to_string_lossy()),
                Self::excluded_dirs(py)?,
                monitoring.getattr("DISABLE")?.unbind(),
                monitoring.clone().unbind(),
                // pytest-rs's own files: the data file and the cache
                // directory, rewritten by every run.
                [state.data_path.clone(), state.rootdir.join(cache_dir)]
                    .iter()
                    .map(|p| p.to_string_lossy().into_owned())
                    .collect(),
                Some(Self::with_sep(
                    &pytest_rs_core::python::shim_root().to_string_lossy(),
                )),
            ),
        )?;
        // Bracket each project module's execution through the import system,
        // so its import-time code also has a context of its own.
        let hook = pyo3::types::PyModule::from_code(
            py,
            &std::ffi::CString::new(include_str!("import_hook.py")).expect("no NUL"),
            c"_pytest_rs_affected_import_hook.py",
            c"_pytest_rs_affected_import_hook",
        )?;
        state.import_finder = Some(
            hook.call_method1(
                "install",
                (
                    collector.bind(py),
                    state.rootdir.to_string_lossy().into_owned(),
                ),
            )?
            .unbind(),
        );

        // Audit hooks cannot be removed: the collector goes inert at the end
        // of the session instead.
        py.import("sys")?
            .call_method1("addaudithook", (collector.bind(py).getattr("audit")?,))?;
        let py_start = monitoring.getattr("events")?.getattr("PY_START")?;
        monitoring.call_method1(
            "register_callback",
            (tool_id, &py_start, collector.bind(py).getattr("py_start")?),
        )?;
        monitoring.call_method1("set_events", (tool_id, &py_start))?;
        state.children_env = Some(Self::children_env(state, py, cache_dir)?);
        Self::arm_children(state, py, &collector)?;
        state.collector = Some(collector);
        state.tool_id = tool_id;
        Ok(())
    }

    /// The .pth line that starts the child recorder in Python subprocesses:
    /// a no-op unless this session exported the activation variables.
    const PTH_LINE: &str = "import os, runpy; os.environ.get(\"PYTEST_RS_AFFECTED_OUT\") and os.environ.get(\"PYTEST_RS_AFFECTED_CHILD\") and runpy.run_path(os.environ[\"PYTEST_RS_AFFECTED_CHILD\"], run_name=\"pytest_rs_affected_child\")\n";

    /// The environment a child recorder needs, besides its output directory
    /// and the current context (both set per process).
    fn children_env(
        state: &State,
        py: Python<'_>,
        cache_dir: &str,
    ) -> PyResult<Vec<(&'static str, String)>> {
        let sep = std::path::MAIN_SEPARATOR.to_string();
        let pathsep = if cfg!(windows) { ";" } else { ":" };
        Ok(vec![
            (
                "PYTEST_RS_AFFECTED_ROOT",
                Self::with_sep(&state.rootdir.to_string_lossy()),
            ),
            (
                "PYTEST_RS_AFFECTED_EXCLUDED",
                Self::excluded_dirs(py)?.join(pathsep),
            ),
            (
                "PYTEST_RS_AFFECTED_IGNORED",
                [state.data_path.clone(), state.rootdir.join(cache_dir)]
                    .iter()
                    .map(|p| {
                        p.to_string_lossy()
                            .trim_end_matches(sep.as_str())
                            .to_string()
                    })
                    .collect::<Vec<_>>()
                    .join(pathsep),
            ),
        ])
    }

    /// Make Python subprocesses record their own dependencies: a per-process
    /// output directory holding the child recorder, the activation
    /// variables, and the .pth line in the environment's site-packages.
    /// Also called after a `-n` fork, so every worker reads only its own
    /// children's records.
    fn arm_children(
        state: &mut State,
        py: Python<'_>,
        collector: &Py<AffectedCollector>,
    ) -> PyResult<()> {
        let out = std::env::temp_dir().join(format!("pytest-rs-affected-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&out);
        std::fs::create_dir_all(&out)
            .map_err(|e| pyo3::exceptions::PyOSError::new_err(e.to_string()))?;
        let child = out.join("child.py");
        std::fs::write(&child, include_str!("child.py"))
            .map_err(|e| pyo3::exceptions::PyOSError::new_err(e.to_string()))?;
        let environ = py.import("os")?.getattr("environ")?;
        for (key, value) in state.children_env.iter().flatten() {
            environ.set_item(*key, value)?;
        }
        environ.set_item("PYTEST_RS_AFFECTED_CHILD", child.to_string_lossy())?;
        environ.set_item("PYTEST_RS_AFFECTED_OUT", out.to_string_lossy())?;
        // Children started with sys.executable run this environment's Python,
        // which processes its own site-packages .pth files.
        let mut site_dirs: Vec<PathBuf> = Vec::new();
        if let Ok(paths) = py.import("sysconfig")?.call_method0("get_paths")
            && let Ok(Some(purelib)) = paths
                .get_item("purelib")
                .map(|p| p.extract::<String>().ok())
        {
            site_dirs.push(PathBuf::from(purelib));
        }
        if let Ok(venv) = std::env::var("VIRTUAL_ENV")
            && let Ok(entries) = std::fs::read_dir(Path::new(&venv).join("lib"))
        {
            site_dirs.extend(
                entries
                    .filter_map(Result::ok)
                    .map(|e| e.path().join("site-packages"))
                    .filter(|dir| dir.is_dir()),
            );
        }
        for dir in site_dirs {
            let _ = std::fs::write(dir.join("pytest-rs-affected.pth"), Self::PTH_LINE);
        }
        state.children_out = Some(out);
        collector.borrow(py).arm_children(py)
    }

    /// Stop children from recording, and fold the records they wrote into
    /// the session's recording: their code and inputs count toward the
    /// context that started them, and a context whose Python children all
    /// came back complete is no longer incomplete.
    fn fold_children(state: &mut State, py: Python<'_>, recording: &mut Recording) {
        if let Ok(environ) = py.import("os").and_then(|os| os.getattr("environ")) {
            for key in ["PYTEST_RS_AFFECTED_OUT", "PYTEST_RS_AFFECTED_CHILD"] {
                let _ = environ.call_method1("pop", (key, py.None()));
            }
        }
        let Some(out) = state.children_out.take() else {
            return;
        };
        let records: Vec<serde_json::Value> = std::fs::read_dir(&out)
            .map(|entries| {
                entries
                    .filter_map(Result::ok)
                    .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
                    .filter_map(|e| std::fs::read(e.path()).ok())
                    .filter_map(|bytes| serde_json::from_slice(&bytes).ok())
                    .collect()
            })
            .unwrap_or_default();
        let _ = std::fs::remove_dir_all(&out);
        ChildRecords::fold(recording, &records);
    }

    /// Make the top of the context stack current.
    fn sync_context(state: &State, py: Python<'_>) {
        if let Some(collector) = &state.collector {
            let context = state.stack.last().copied().unwrap_or(GLOBAL_CONTEXT);
            if let Err(err) = collector.borrow(py).switch(py, context) {
                err.print(py);
            }
        }
    }

    fn context_for(
        collector: &Py<AffectedCollector>,
        py: Python<'_>,
        map: &mut HashMap<String, usize>,
        key: &str,
    ) -> usize {
        *map.entry(key.to_string())
            .or_insert_with(|| collector.borrow(py).new_context())
    }

    fn setup_context(state: &mut State, py: Python<'_>, setup: &ScopedSetup) -> Option<usize> {
        let collector = state.collector.as_ref()?;
        Some(
            *state
                .setup_contexts
                .entry(setup.clone())
                .or_insert_with(|| collector.borrow(py).new_context()),
        )
    }

    fn snapshot(state: &State) -> Vec<SnapEntry> {
        let vouched =
            |entry: &SnapEntry| Self::vouched(state, &entry.path, entry.commit.as_deref());
        // Entries hashed now name HEAD when the file is tracked and clean.
        let clean: Option<(String, HashSet<String>)> = state
            .git
            .as_ref()
            .and_then(|git| Some((git.head().to_string(), git.clean_at_head()?)));
        let builtin_ignore = Globs::builtin(BUILTIN_IGNORE);
        let builtin_full = Globs::builtin(BUILTIN_FULL_RUN_ON);
        let own = Self::rel(&state.rootdir, &state.data_path)
            .into_iter()
            .collect();
        let snapshot = Snapshot {
            rootdir: &state.rootdir,
            norecurse: state
                .norecursedirs
                .iter()
                .filter_map(|p| glob::Pattern::new(p).ok())
                .collect(),
            ignore: vec![&builtin_ignore, &state.config.ignore],
            include_py: [&builtin_full, &state.config.full_run_on]
                .into_iter()
                .chain(state.config.file_triggers.iter().map(|(key, _)| key))
                .collect(),
            own_files: own,
            vouched: Some(&vouched),
            clean_at_head: clean.as_ref().map(|(head, set)| (head.as_str(), set)),
        };
        let previous = state
            .stored
            .as_ref()
            .map(|d| d.snapshot.as_slice())
            .unwrap_or_default();
        snapshot.take(previous)
    }

    /// Test files named with a `::` node id on the command line, as
    /// rootdir-relative node id prefixes.
    fn explicit_nodeids(ctx: &HookContext) -> Vec<String> {
        ctx.config
            .paths
            .iter()
            .filter_map(|arg| {
                let (path, rest) = arg.split_once("::")?;
                let abs = ctx.config.invocation_dir.join(path);
                let abs = abs.canonicalize().unwrap_or(abs);
                let root = ctx
                    .config
                    .rootdir
                    .canonicalize()
                    .unwrap_or(ctx.config.rootdir.clone());
                let rel = Self::rel(&root, &abs)?;
                Some(format!("{rel}::{rest}"))
            })
            .collect()
    }

    /// The controller of a `-n` run: it runs no tests itself.
    fn is_dist_controller(ctx: &HookContext) -> bool {
        !ctx.config.is_worker()
            && !ctx.config.collect_only
            && ctx.session.dist_workers_resolved.flatten().is_some()
    }

    /// Which stored states git vouches for: one `git diff` per distinct
    /// commit they name.
    fn ensure_unchanged(state: &mut State) {
        if state.unchanged.is_some() {
            return;
        }
        let commits: Vec<String> = state
            .stored
            .as_ref()
            .map(|data| {
                data.files
                    .values()
                    .filter_map(|e| e.commit.map(|c| data.strings.resolve(c).to_string()))
                    .chain(data.snapshot.iter().filter_map(|e| e.commit.clone()))
                    .collect()
            })
            .unwrap_or_default();
        state.unchanged = Some(Unchanged::new(
            state.git.as_ref(),
            commits.iter().map(String::as_str),
        ));
    }

    fn vouched(state: &State, path: &str, commit: Option<&str>) -> bool {
        state
            .unchanged
            .as_ref()
            .is_some_and(|u| u.contains(state.git.as_ref(), commit, path))
    }

    /// The commit (interned) a file state recorded now is the content of:
    /// HEAD, when git reports the file tracked and clean, HEAD did not move
    /// during the session, and the file was not modified since it started.
    fn commit_for(
        state: &State,
        end: Option<&(String, HashSet<String>)>,
        data: &mut Data,
        rel: &str,
    ) -> Option<u32> {
        let (head, clean) = end?;
        if state.git.as_ref().map(Git::head) != Some(head.as_str()) || !clean.contains(rel) {
            return None;
        }
        let modified = std::fs::metadata(state.rootdir.join(rel))
            .ok()?
            .modified()
            .ok()?;
        (modified < state.started?).then(|| data.strings.intern(head))
    }

    /// HEAD and the files clean at it, at the end of a recording session.
    fn git_at_end(state: &State) -> Option<(String, HashSet<String>)> {
        state.git.as_ref()?;
        let git = Git::open(&state.rootdir)?;
        let clean = git.clean_at_head()?;
        Some((git.head().to_string(), clean))
    }

    /// Build the selection plan once (needs usable data).
    fn ensure_plan(state: &mut State, ctx: &HookContext) {
        if state.plan.is_some() {
            return;
        }
        Self::ensure_unchanged(state);
        if state.snapshot.is_none() {
            state.snapshot = Some(Self::snapshot(state));
        }
        let Some(data) = state.stored.as_ref() else {
            return;
        };
        let vouched = |path: &str, commit: Option<&str>| Self::vouched(state, path, commit);
        let plan = Plan::build(
            data,
            &state.rootdir,
            &state.config,
            state.snapshot.as_deref().unwrap_or_default(),
            Self::explicit_nodeids(ctx),
            &vouched,
        );
        if plan.full_reason.is_some() {
            // A full-run trigger changed since the stored entries were
            // recorded: tests this run does not cover must not keep them.
            state.discard_stored = true;
        }
        state.plan = Some(plan);
    }

    /// Item-level selection over the imported files.
    fn select(
        state: &mut State,
        ctx: &mut HookContext,
        items: &mut Vec<TestItem>,
    ) -> Option<String> {
        let (Some(data), Some(plan)) = (state.stored.as_ref(), state.plan.as_mut()) else {
            return Some(format!(
                "running all {} tests ({})",
                items.len(),
                state.unusable.as_deref().unwrap_or("no data")
            ));
        };
        if let Some(reason) = &plan.full_reason {
            return Some(format!("running all {} tests ({reason})", items.len()));
        }
        if data.global_incomplete && state.config.incomplete == Incomplete::Run {
            // Every test is incomplete: say why instead of listing them as
            // selected one by one.
            return Some(format!(
                "running all {} tests (incomplete deps: a subprocess was started outside \
                 any test; set incomplete = \"trust\" to rely on the recorded dependencies)",
                items.len()
            ));
        }
        let mut reasons: HashMap<Reason, usize> = HashMap::new();
        let (keep, drop): (Vec<TestItem>, Vec<TestItem>) =
            std::mem::take(items).into_iter().partition(|item| {
                let path = Self::item_path(&state.rootdir, item);
                match plan.reason(data, &state.config, &item.nodeid, &path) {
                    Some(reason) => {
                        *reasons.entry(reason).or_default() += 1;
                        true
                    }
                    None => false,
                }
            });
        *items = keep;
        let deselected = drop.len();
        ctx.session.deselected_items.extend(drop);
        if items.is_empty() && deselected > 0 {
            ctx.session.no_tests_ok = true;
        }
        let mut reasons: Vec<(Reason, usize)> = reasons.into_iter().collect();
        reasons.sort();
        let reasons: Vec<String> = reasons
            .iter()
            .map(|(reason, n)| format!("{n} {}", reason.label()))
            .collect();
        Some(format!(
            "{} selected{}, {deselected} deselected",
            items.len(),
            if reasons.is_empty() {
                String::new()
            } else {
                format!(" ({})", reasons.join(", "))
            }
        ))
    }

    /// Selection (select/auto modes) or, when only recording, the check
    /// whether stored entries survive (a full-run trigger changed).
    fn apply_mode(
        state: &mut State,
        mode: Mode,
        ctx: &mut HookContext,
        items: &mut Vec<TestItem>,
    ) -> PyResult<()> {
        Self::ensure_plan(state, ctx);
        if mode.selects()
            && let Some(summary) = Self::select(state, ctx, items)
            && !ctx.config.no_terminal()
        {
            println!("affected: {summary}");
        }
        Ok(())
    }

    /// Rule 3: per imported test file, a group with the module unit
    /// of every project module its own import statements name. These are
    /// the modules a cache hit hid from the file's import context.
    fn static_import_groups(
        state: &State,
        py: Python<'_>,
        recording: &Recording,
        data: &mut Data,
        end: Option<&(String, HashSet<String>)>,
    ) -> HashMap<String, u32> {
        let seen: HashMap<&str, &Option<Vec<u8>>> = recording
            .files
            .iter()
            .map(|(abs, content)| (abs.as_str(), content))
            .collect();
        let Ok(modules) = py.import("sys").and_then(|sys| sys.getattr("modules")) else {
            return HashMap::new();
        };
        let mut out = HashMap::new();
        let paths: BTreeSet<&String> = state
            .collect_contexts
            .keys()
            .chain(state.file_modules.keys())
            .collect();
        for path in paths {
            let Ok(source) = std::fs::read_to_string(state.rootdir.join(path)) else {
                continue;
            };
            let module = state.file_modules.get(path).map(String::as_str);
            let names = StaticImports::of(&source, module, path.ends_with("__init__.py"));
            let mut deps: Vec<Dep> = Vec::new();
            for name in names {
                let file: Option<String> = modules
                    .get_item(&name)
                    .ok()
                    .and_then(|m| m.getattr("__file__").ok())
                    .and_then(|f| f.extract().ok());
                let Some(file) = file else { continue };
                let Some(rel) = Self::rel(&state.rootdir, Path::new(&file)) else {
                    continue;
                };
                if !rel.ends_with(".py") || rel.split('/').any(|p| p == "site-packages") {
                    continue;
                }
                let bytes = match seen.get(file.as_str()) {
                    Some(Some(bytes)) => bytes.clone(),
                    _ => match std::fs::read(&file) {
                        Ok(bytes) => bytes,
                        Err(_) => continue,
                    },
                };
                let units = std::str::from_utf8(&bytes).ok().and_then(FileUnits::parse);
                let sid = data.strings.intern(&rel);
                let content_hash = FileUnits::content_hash(&bytes);
                let entry_units = units.as_ref().map(|f| {
                    let mut units: Vec<(u32, u64)> = f
                        .units
                        .iter()
                        .map(|(q, c)| (data.strings.intern(q), *c))
                        .collect();
                    units.sort_unstable();
                    units
                });
                if !data.files.contains_key(&sid) {
                    let commit = Self::commit_for(state, end, data, &rel);
                    data.files.insert(
                        sid,
                        FileEntry {
                            content_hash,
                            units: entry_units,
                            commit,
                        },
                    );
                }
                let (qualname, checksum) =
                    match units.as_ref().and_then(|f| f.units.get(MODULE_UNIT)) {
                        Some(&checksum) => (MODULE_UNIT, checksum),
                        None => (FILE_UNIT, content_hash),
                    };
                deps.push(Dep {
                    file: sid,
                    qualname: data.strings.intern(qualname),
                    checksum,
                });
            }
            if !deps.is_empty() {
                out.insert(path.clone(), data.add_group(deps));
            }
        }
        out
    }

    /// Per imported test file, the import-time dependencies of every
    /// project module reachable from it through imports, whichever context
    /// first imported them: modules it imported (first or from the cache)
    /// and, transitively, the modules those imported. Returns the group and
    /// whether any of those imports started a subprocess.
    fn import_closure_groups(
        state: &State,
        py: Python<'_>,
        recording: &Recording,
        graph: &ImportGraph,
        effective: &[(HashSet<Dep>, bool)],
        data: &mut Data,
    ) -> HashMap<String, (Option<u32>, bool)> {
        if recording.import_contexts.is_empty() {
            return HashMap::new();
        }
        let seen: HashMap<&str, &Option<Vec<u8>>> = recording
            .files
            .iter()
            .map(|(abs, content)| (abs.as_str(), content))
            .collect();
        // Module name and file of everything loaded, for resolving import
        // statements to the import contexts of project modules.
        let mut by_name: HashMap<String, String> = HashMap::new();
        let mut name_of: HashMap<String, String> = HashMap::new();
        if let Ok(modules) = py
            .import("sys")
            .and_then(|sys| sys.getattr("modules"))
            .and_then(|m| m.call_method0("copy"))
            .and_then(|m| m.call_method0("items"))
            .and_then(|items| items.try_iter())
        {
            for item in modules.flatten() {
                let Ok((name, module)) = item.extract::<(String, Bound<'_, PyAny>)>() else {
                    continue;
                };
                let Some(file) = module
                    .getattr("__file__")
                    .ok()
                    .and_then(|f| f.extract::<String>().ok())
                else {
                    continue;
                };
                name_of.entry(file.clone()).or_insert_with(|| name.clone());
                by_name.insert(name, file);
            }
        }
        // Parsed once per module, however many test files reach it.
        let parsed: std::cell::RefCell<HashMap<String, Vec<usize>>> = Default::default();
        let static_edges = |file: &str| -> Vec<usize> {
            if let Some(edges) = parsed.borrow().get(file) {
                return edges.clone();
            }
            let source = match seen.get(file) {
                Some(Some(bytes)) => String::from_utf8_lossy(bytes).into_owned(),
                _ => std::fs::read_to_string(file).unwrap_or_default(),
            };
            let module = name_of.get(file).map(String::as_str);
            let edges: Vec<usize> =
                StaticImports::of(&source, module, file.ends_with("__init__.py"))
                    .iter()
                    .filter_map(|name| by_name.get(name))
                    .filter_map(|target| recording.import_contexts.get(target).copied())
                    .collect();
            parsed.borrow_mut().insert(file.to_string(), edges.clone());
            edges
        };
        // Test files that reach the same modules share one group.
        let mut by_closure: HashMap<Vec<usize>, (Option<u32>, bool)> = HashMap::new();
        let mut out = HashMap::new();
        let paths: BTreeSet<&String> = state
            .collect_contexts
            .keys()
            .chain(state.file_modules.keys())
            .collect();
        for path in paths {
            let abs = state.rootdir.join(path).to_string_lossy().into_owned();
            let roots: Vec<usize> = recording
                .import_contexts
                .get(&abs)
                .copied()
                .into_iter()
                .chain(
                    state
                        .collect_contexts
                        .get(path)
                        .map(|c| graph.children(*c))
                        .unwrap_or_default(),
                )
                .chain(static_edges(&abs))
                .collect();
            let mut reached: Vec<usize> = graph.closure(roots, &static_edges).into_iter().collect();
            if reached.is_empty() {
                continue;
            }
            reached.sort_unstable();
            let entry = by_closure.entry(reached).or_insert_with_key(|reached| {
                let mut deps: HashSet<Dep> = HashSet::new();
                let mut incomplete = false;
                for context in reached {
                    deps.extend(effective[*context].0.iter().copied());
                    incomplete |= effective[*context].1;
                }
                let group = (!deps.is_empty()).then(|| data.add_group(deps.into_iter().collect()));
                (group, incomplete)
            });
            out.insert(path.clone(), *entry);
        }
        out
    }

    /// Turn the session's recording into data entries, merged into the
    /// stored data (or a fresh file), and write it.
    /// Fold the session's recording into `data` (the stored data, or a fresh
    /// one) and return it with the number of tests recorded.
    fn record(
        state: &mut State,
        ctx: &mut HookContext,
        recording: Recording,
        mut data: Data,
    ) -> (Data, usize) {
        data.header = state.header.clone().expect("header computed at configure");

        // Files: units at first sight, and the deps each code object implies.
        let file_units: Vec<(Option<u32>, Option<FileUnits>, u64)> = recording
            .files
            .iter()
            .map(|(abs, content)| {
                let rel = Self::rel(&state.rootdir, Path::new(abs));
                let sid = rel.as_deref().map(|rel| data.strings.intern(rel));
                let (units, hash) = match content {
                    Some(bytes) => (
                        std::str::from_utf8(bytes).ok().and_then(FileUnits::parse),
                        FileUnits::content_hash(bytes),
                    ),
                    None => (None, 0),
                };
                (sid, units, hash)
            })
            .collect();
        let end = Self::git_at_end(state);
        for (sid, units, hash) in &file_units {
            let Some(sid) = sid else { continue };
            let rel = data.strings.resolve(*sid).to_string();
            let commit = Self::commit_for(state, end.as_ref(), &mut data, &rel);
            let units = units.as_ref().map(|f| {
                let mut units: Vec<(u32, u64)> = f
                    .units
                    .iter()
                    .map(|(q, c)| (data.strings.intern(q), *c))
                    .collect();
                units.sort_unstable();
                units
            });
            data.files.insert(
                *sid,
                FileEntry {
                    content_hash: *hash,
                    units,
                    commit,
                },
            );
        }
        let code_deps: Vec<Vec<Dep>> = recording
            .codes
            .iter()
            .map(|(file, co_qualname)| {
                let (Some(sid), units, hash) = &file_units[*file as usize] else {
                    return Vec::new();
                };
                let dep = |data: &mut Data, qualname: &str, checksum: u64| Dep {
                    file: *sid,
                    qualname: data.strings.intern(qualname),
                    checksum,
                };
                match units {
                    None => vec![dep(&mut data, FILE_UNIT, *hash)],
                    Some(units) => {
                        let unit = units.unit_for_qualname(co_qualname);
                        if unit == FILE_UNIT {
                            let mut deps = vec![dep(&mut data, FILE_UNIT, *hash)];
                            if let Some(&c) = units.units.get(MODULE_UNIT) {
                                deps.push(dep(&mut data, MODULE_UNIT, c));
                            }
                            return deps;
                        }
                        std::iter::once(unit.clone())
                            .chain(FileUnits::enclosing(&unit))
                            .filter_map(|q| units.units.get(&q).map(|&c| (q, c)))
                            .map(|(q, c)| dep(&mut data, &q, c))
                            .collect()
                    }
                }
            })
            .collect();
        // Non-Python inputs: content or listing hash at first sight.
        let input_deps: Vec<Option<Dep>> = recording
            .inputs
            .iter()
            .map(|(abs, kind, hash)| {
                let rel = Self::rel(&state.rootdir, Path::new(abs))?;
                Some(Dep {
                    file: data.strings.intern(&rel),
                    qualname: data.strings.intern(kind.qualname()),
                    checksum: *hash,
                })
            })
            .collect();
        let own: Vec<HashSet<Dep>> = recording
            .contexts
            .iter()
            .enumerate()
            .map(|(context, codes)| {
                codes
                    .iter()
                    .flat_map(|code| code_deps[*code as usize].iter().copied())
                    .chain(
                        recording
                            .context_inputs
                            .get(context)
                            .into_iter()
                            .flatten()
                            .filter_map(|input| input_deps[*input as usize]),
                    )
                    .collect()
            })
            .collect();
        // A module executed while a context was current is part of that
        // context too (its import context only adds a finer grain).
        let graph = ImportGraph::new(&recording);
        let effective: Vec<(HashSet<Dep>, bool)> = (0..own.len())
            .map(|context| graph.effective(context, &own))
            .collect();
        let groups: Vec<Option<u32>> = effective
            .iter()
            .map(|(deps, _)| {
                (!deps.is_empty()).then(|| data.add_group(deps.iter().copied().collect()))
            })
            .collect();
        let incomplete_contexts: HashSet<usize> = effective
            .iter()
            .enumerate()
            .filter(|(_, (_, incomplete))| *incomplete)
            .map(|(context, _)| context)
            .collect();
        let closure_groups =
            Self::import_closure_groups(state, ctx.py, &recording, &graph, &effective, &mut data);
        let static_groups =
            Self::static_import_groups(state, ctx.py, &recording, &mut data, end.as_ref());

        // Tests that ran.
        let mut outcomes: HashMap<&str, (bool, f64)> = HashMap::new();
        for report in &ctx.session.reports {
            let entry = outcomes.entry(report.nodeid.as_str()).or_default();
            entry.0 |= report.outcome == Outcome::Failed && !report.rerun;
            entry.1 += report.duration.as_secs_f64();
        }
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let mut recorded = 0usize;
        for nodeid in &state.to_run {
            let Some(&(failed, duration)) = outcomes.get(nodeid.as_str()) else {
                continue;
            };
            // A test reported without reaching the runtest_setup hook (a
            // skip mark) ran none of its own code: its file's import and
            // the global context are its dependencies.
            let (run_path, own_context, setups) = match state.runs.get(nodeid) {
                Some(run) => (run.path.clone(), Some(run.context), run.setups.clone()),
                None => (
                    nodeid.split("::").next().unwrap_or(nodeid).to_string(),
                    None,
                    BTreeSet::new(),
                ),
            };
            let contexts: Vec<usize> = std::iter::once(GLOBAL_CONTEXT)
                .chain(state.collect_contexts.get(&run_path).copied())
                .chain(setups)
                .chain(own_context)
                .collect();
            let (closure_group, closure_incomplete) = closure_groups
                .get(&run_path)
                .copied()
                .unwrap_or((None, false));
            let incomplete =
                closure_incomplete || contexts.iter().any(|c| incomplete_contexts.contains(c));
            let test_groups: Vec<u32> = contexts
                .iter()
                .filter_map(|c| groups.get(*c).copied().flatten())
                .chain(static_groups.get(&run_path).copied())
                .chain(closure_group)
                .collect();
            let id = data.strings.intern(nodeid);
            let path = data.strings.intern(&run_path);
            data.tests.insert(
                id,
                TestEntry {
                    path,
                    failed,
                    duration,
                    groups: test_groups,
                    incomplete,
                    recorded: now,
                },
            );
            recorded += 1;
        }
        // Test files imported this session: what they collected and the
        // groups every one of their tests depends on.
        let imported: BTreeSet<&String> = state
            .collect_contexts
            .keys()
            .chain(state.file_nodeids.keys())
            .collect();
        for path in imported {
            // The file's whole content: a test added to it changes no unit
            // an existing test depends on, but must be collected.
            // Taken from the collector's first sight (the content that was
            // imported), not from disk now.
            let own = data
                .strings
                .get(path)
                .and_then(|file| Some((file, data.files.get(&file)?.content_hash)))
                .map(|(file, content_hash)| {
                    let qualname = data.strings.intern(FILE_UNIT);
                    data.add_group(vec![Dep {
                        file,
                        qualname,
                        checksum: content_hash,
                    }])
                });
            let groups: Vec<u32> = std::iter::once(GLOBAL_CONTEXT)
                .chain(state.collect_contexts.get(path).copied())
                .filter_map(|c| groups.get(c).copied().flatten())
                .chain(static_groups.get(path).copied())
                .chain(closure_groups.get(path).and_then(|(g, _)| *g))
                .chain(own)
                .collect();
            let nodeids: Vec<u32> = state
                .file_nodeids
                .get(path)
                .into_iter()
                .flatten()
                .map(|nodeid| data.strings.intern(nodeid))
                .collect();
            let id = data.strings.intern(path);
            data.file_records.insert(
                id,
                FileRecord {
                    groups,
                    nodeids,
                    recorded: now,
                },
            );
        }
        data.global_incomplete = incomplete_contexts.contains(&GLOBAL_CONTEXT);
        data.snapshot = match state.snapshot.take() {
            Some(snapshot) => snapshot,
            None => Self::snapshot(state),
        };
        let ran: HashSet<String> = state.to_run.iter().cloned().collect();
        Self::drop_stale(state, &mut data, &ran);
        (data, recorded)
    }

    /// Before writing: drop the entries tests a file_triggers entry forced
    /// but this run left out (-k), so they run as new next time, and the
    /// entries whose test file is gone.
    fn drop_stale(state: &State, data: &mut Data, ran: &HashSet<String>) {
        if let Some(plan) = &state.plan {
            let left_out: Vec<u32> = data
                .tests
                .iter()
                .filter(|(id, entry)| {
                    let path = data.strings.resolve(entry.path);
                    plan.triggered.iter().any(|g| g.matches(path))
                        && !ran.contains(data.strings.resolve(**id))
                })
                .map(|(id, _)| *id)
                .collect();
            for id in left_out {
                data.tests.remove(&id);
            }
        }
        let rootdir = &state.rootdir;
        let gone: Vec<u32> = data
            .tests
            .iter()
            .filter(|(_, entry)| !rootdir.join(data.strings.resolve(entry.path)).exists())
            .map(|(id, _)| *id)
            .collect();
        for id in gone {
            data.tests.remove(&id);
        }
        let strings = &data.strings;
        data.file_records
            .retain(|path, _| rootdir.join(strings.resolve(*path)).exists());
    }

    fn save(data: &Data, path: &Path) -> PyResult<()> {
        data.save(path).map_err(|e| {
            pyo3::exceptions::PyOSError::new_err(format!("cannot write {}: {e}", path.display()))
        })
    }

    /// The stored data to record into, or a fresh one when a full-run
    /// trigger changed (or there is none).
    fn base_data(state: &mut State) -> Data {
        let header = state.header.clone().expect("header computed at configure");
        match state.stored.take() {
            Some(data) if !state.discard_stored => data,
            _ => Data::new(header),
        }
    }

    /// The controller of a `-n` run: fold every worker's recording into
    /// the stored data and write it.
    fn merge_workers(state: &mut State, ctx: &mut HookContext) -> PyResult<usize> {
        let partials = std::mem::take(&mut state.partials);
        if state.plan.is_none() {
            Self::ensure_plan(state, ctx);
        }
        if partials.iter().any(|(_, discard)| *discard) {
            state.discard_stored = true;
        }
        let mut data = Self::base_data(state);
        data.header = state.header.clone().expect("header computed at configure");
        let mut ran: HashSet<String> = HashSet::new();
        let mut recorded = 0;
        for (partial, _) in &partials {
            ran.extend(
                partial
                    .tests
                    .keys()
                    .map(|id| partial.strings.resolve(*id).to_string()),
            );
            recorded += partial.tests.len();
            data.merge(partial);
        }
        Self::drop_stale(state, &mut data, &ran);
        Self::save(&data, &state.data_path)?;
        Ok(recorded)
    }
}

impl Plugin for AffectedPlugin {
    fn name(&self) -> &str {
        "affected"
    }

    fn pytest_addoption(&self, parser: &mut OptionParser) {
        parser.add_option(
            OptDef::optional_value(
                "--affected",
                "Run only the tests affected by changes since the recorded run \
             ('auto', the default: select and record what ran; 'record': run \
             as usual and record; 'select': select without recording). \
             The mode only binds as --affected=MODE",
            )
            .with_require_equals(),
        );
        parser.add_option(OptDef::flag(
            "--affected-merge",
            "Merge the --affected data files given as arguments into --affected-data \
             (sharded CI runs), and run no tests",
        ));
        parser.add_option(OptDef::value(
            "--affected-data",
            None,
            "Path of the --affected data file (default: .pytest-rs-affected in the rootdir)",
        ));
    }

    fn pytest_configure(&mut self, ctx: &mut HookContext) -> PyResult<()> {
        let py = ctx.py;
        let mode = Self::parse_mode(ctx.config.get_values("affected"))
            .map_err(|msg| pytest_rs_core::python::usage_error(py, &msg))?;
        let mut state = self.lock();
        state.mode = Some(mode);
        if mode == Mode::Off {
            return Ok(());
        }
        state.rootdir = ctx.config.rootdir.clone();
        state.data_path = match ctx.config.get_value("affected-data") {
            Some(path) => ctx.config.invocation_dir.join(path),
            None => state.rootdir.join(Self::DEFAULT_DATA_FILE),
        };
        state.config = AffectedConfig::load(&state.rootdir)
            .map_err(|msg| pytest_rs_core::python::usage_error(py, &msg))?;
        let norecurse = ctx.config.get_ini_lines("norecursedirs");
        state.norecursedirs = if norecurse.is_empty() {
            Snapshot::DEFAULT_NORECURSE
                .iter()
                .map(|s| s.to_string())
                .collect()
        } else {
            norecurse
                .iter()
                .flat_map(|line| line.split_whitespace())
                .map(str::to_string)
                .collect()
        };
        let header = Self::current_header(py)?;
        match Data::load(&state.data_path) {
            Err(LoadError::Missing) => state.unusable = Some("no data".into()),
            Err(LoadError::OtherFormat) => state.unusable = Some("data format changed".into()),
            Err(LoadError::Unreadable) => state.unusable = Some("unreadable data".into()),
            Ok(data) => match data.header.mismatch(&header) {
                Some(reason) => state.unusable = Some(reason.into()),
                None => state.stored = Some(data),
            },
        }
        state.header = Some(header);
        state.git = Git::open(&state.rootdir);
        state.started = Some(std::time::SystemTime::now());
        if mode.records() {
            let cache_dir = ctx
                .config
                .get_ini("cache_dir")
                .unwrap_or(".pytest_cache")
                .to_string();
            Self::start_collector(&mut state, py, &cache_dir)?;
        }
        Ok(())
    }

    fn pytest_report_header(&self, _ctx: &mut HookContext) -> PyResult<Vec<String>> {
        let state = self.lock();
        let Some(mode) = state.mode.filter(|m| *m != Mode::Off) else {
            return Ok(Vec::new());
        };
        let mode = match mode {
            Mode::Auto => "auto",
            Mode::Record => "record",
            Mode::Select => "select",
            Mode::Off => unreachable!(),
        };
        let data = match (&state.stored, &state.unusable) {
            (Some(data), _) => format!("{} tests recorded", data.tests.len()),
            (None, Some(reason)) => reason.clone(),
            (None, None) => "no data".into(),
        };
        let path = Self::rel(&state.rootdir, &state.data_path)
            .unwrap_or_else(|| state.data_path.display().to_string());
        Ok(vec![format!("affected: {mode}, {path} ({data})")])
    }

    fn pytest_collect_module_start(&self, py: Python<'_>, path: &Path) {
        let mut state = self.lock();
        let Some(collector) = state.collector.as_ref().map(|c| c.clone_ref(py)) else {
            return;
        };
        let Some(rel) = Self::rel(&state.rootdir, path) else {
            return;
        };
        let context = Self::context_for(&collector, py, &mut state.collect_contexts, &rel);
        state.stack = vec![context];
        Self::sync_context(&state, py);
    }

    fn pytest_collect_module_finish(&self, py: Python<'_>, _path: &Path) {
        let mut state = self.lock();
        if state.collector.is_none() {
            return;
        }
        state.stack.clear();
        Self::sync_context(&state, py);
    }

    fn pytest_collection_select_files(
        &mut self,
        ctx: &mut HookContext,
        files: &mut Vec<PathBuf>,
    ) -> PyResult<()> {
        let mut state = self.lock();
        if !state.mode.is_some_and(Mode::selects) {
            return Ok(());
        }
        Self::ensure_plan(&mut state, ctx);
        // A -n controller never sees the workers' selection: with usable
        // data, an empty run is the selection's doing.
        if !ctx.config.is_worker()
            && state.stored.is_some()
            && ctx
                .config
                .numprocesses_spec()
                .is_some_and(|spec| spec.trim() != "0")
        {
            ctx.session.no_tests_ok = true;
        }
        let state = &mut *state;
        let (Some(data), Some(plan)) = (state.stored.as_ref(), state.plan.as_mut()) else {
            return Ok(());
        };
        let before = files.len();
        let rootdir = state.rootdir.clone();
        files.retain(|file| {
            let path = Self::rel(&rootdir, file).unwrap_or_default();
            !plan.file_unchanged(data, &state.config, &path)
        });
        let skipped = before - files.len();
        if skipped > 0 {
            // Nothing left to run because of the selection is a success.
            ctx.session.no_tests_ok = true;
            if !ctx.config.no_terminal() {
                println!("affected: {skipped} of {before} test files unchanged, not imported");
            }
        }
        Ok(())
    }

    fn pytest_collection_modifyitems(
        &self,
        ctx: &mut HookContext,
        items: &mut Vec<TestItem>,
    ) -> PyResult<()> {
        let mut state = self.lock();
        let Some(mode) = state.mode.filter(|m| *m != Mode::Off) else {
            return Ok(());
        };
        for item in items.iter().chain(ctx.session.deselected_items.iter()) {
            let path = Self::item_path(&state.rootdir, item);
            state
                .file_modules
                .entry(path.clone())
                .or_insert_with(|| item.module_name.clone());
            state
                .file_nodeids
                .entry(path)
                .or_default()
                .push(item.nodeid.clone());
        }
        let result = Self::apply_mode(&mut state, mode, ctx, items);
        state.to_run = items.iter().map(|item| item.nodeid.clone()).collect();
        result
    }

    fn pytest_runtest_setup(&self, ctx: &mut HookContext, item: &TestItem) -> PyResult<()> {
        let mut state = self.lock();
        let Some(collector) = state.collector.as_ref().map(|c| c.clone_ref(ctx.py)) else {
            return Ok(());
        };
        let context = collector.borrow(ctx.py).new_context();
        let path = Self::item_path(&state.rootdir, item);
        state.runs.insert(
            item.nodeid.clone(),
            TestRun {
                path,
                context,
                setups: BTreeSet::new(),
            },
        );
        state.current_test = Some(item.nodeid.clone());
        state.stack = vec![context];
        Self::sync_context(&state, ctx.py);
        Ok(())
    }
    fn pytest_scoped_setup_start(&self, py: Python<'_>, setup: &ScopedSetup, _item: &TestItem) {
        let mut state = self.lock();
        let Some(context) = Self::setup_context(&mut state, py, setup) else {
            return;
        };
        state.stack.push(context);
        Self::sync_context(&state, py);
    }

    fn pytest_scoped_setup_finish(
        &self,
        py: Python<'_>,
        setup: &ScopedSetup,
        item: &TestItem,
        cached: bool,
    ) {
        let mut state = self.lock();
        let Some(context) = Self::setup_context(&mut state, py, setup) else {
            return;
        };
        if !cached {
            if let Some(pos) = state.stack.iter().rposition(|c| *c == context) {
                state.stack.truncate(pos);
            }
            Self::sync_context(&state, py);
        }
        if let Some(run) = state.runs.get_mut(&item.nodeid) {
            run.setups.insert(context);
        }
    }

    fn pytest_finalizer_start(&self, py: Python<'_>, finalizer: &PendingFinalizer) {
        let Some(owner) = &finalizer.owner else {
            return;
        };
        let mut state = self.lock();
        let Some(context) = Self::setup_context(&mut state, py, owner) else {
            return;
        };
        state.stack.push(context);
        Self::sync_context(&state, py);
    }

    fn pytest_finalizer_finish(&self, py: Python<'_>, finalizer: &PendingFinalizer) {
        let Some(owner) = &finalizer.owner else {
            return;
        };
        let mut state = self.lock();
        let Some(context) = Self::setup_context(&mut state, py, owner) else {
            return;
        };
        if let Some(pos) = state.stack.iter().rposition(|c| *c == context) {
            state.stack.truncate(pos);
        }
        Self::sync_context(&state, py);
    }

    fn pytest_runtest_teardown(&self, ctx: &mut HookContext, _item: &TestItem) -> PyResult<()> {
        let mut state = self.lock();
        if state.collector.is_none() {
            return Ok(());
        }
        state.current_test = None;
        state.stack.clear();
        Self::sync_context(&state, ctx.py);
        Ok(())
    }

    /// A forked `-n` worker inherited the controller's children output
    /// directory: give it its own, so it folds only its own children.
    fn reinit_post_fork(&mut self, py: Python<'_>) {
        let mut state = self.lock();
        if state.children_out.is_none() {
            return;
        }
        let Some(collector) = state.collector.as_ref().map(|c| c.clone_ref(py)) else {
            return;
        };
        if let Err(err) = Self::arm_children(&mut state, py, &collector) {
            err.print(py);
        }
    }

    fn pytest_worker_dump(&mut self, _ctx: &mut HookContext) -> PyResult<Option<String>> {
        let state = self.lock();
        Ok(state.partial_path.as_ref().map(|path| {
            format!(
                "{}\n{}",
                u8::from(state.discard_stored),
                path.to_string_lossy()
            )
        }))
    }

    fn pytest_worker_load(&mut self, _ctx: &mut HookContext, payload: &str) -> PyResult<()> {
        let Some((discard, path)) = payload.split_once('\n') else {
            return Ok(());
        };
        let path = PathBuf::from(path);
        let data = Data::load(&path);
        let _ = std::fs::remove_file(&path);
        let data = data.map_err(|_| {
            pyo3::exceptions::PyOSError::new_err(format!(
                "cannot read a worker's --affected recording {}",
                path.display()
            ))
        })?;
        self.lock().partials.push((data, discard == "1"));
        Ok(())
    }

    fn pytest_sessionfinish(&mut self, ctx: &mut HookContext, _exit_code: i32) -> PyResult<()> {
        let mut state = self.lock();
        let Some(collector) = state.collector.take() else {
            return Ok(());
        };
        let py = ctx.py;
        let monitoring = py.import("sys")?.getattr("monitoring")?;
        monitoring.call_method1("set_events", (state.tool_id, 0))?;
        monitoring.call_method1("free_tool_id", (state.tool_id,))?;
        if let Some(finder) = state.import_finder.take() {
            let _ = py
                .import("sys")?
                .getattr("meta_path")?
                .call_method1("remove", (finder,));
        }
        collector.borrow(py).disarm_children();
        let mut recording = collector.borrow(py).take();
        Self::fold_children(&mut state, py, &mut recording);
        let recorded = if ctx.config.is_worker() {
            // A -n worker records only its own tests, for the controller to
            // merge (pytest_worker_dump).
            let fresh = Data::new(state.header.clone().expect("header computed at configure"));
            let (data, recorded) = Self::record(&mut state, ctx, recording, fresh);
            let path = state.data_path.with_file_name(format!(
                "{}.worker-{}",
                state
                    .data_path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                std::process::id()
            ));
            Self::save(&data, &path)?;
            state.partial_path = Some(path);
            recorded
        } else if Self::is_dist_controller(ctx) {
            Self::merge_workers(&mut state, ctx)?
        } else {
            let base = Self::base_data(&mut state);
            let (data, recorded) = Self::record(&mut state, ctx, recording, base);
            Self::save(&data, &state.data_path)?;
            recorded
        };
        if !ctx.config.no_terminal() {
            let path = Self::rel(&state.rootdir, &state.data_path)
                .unwrap_or_else(|| state.data_path.display().to_string());
            println!("\naffected: recorded {recorded} tests in {path}");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ruff_tag_matches_workspace() {
        let manifest = include_str!("../../../Cargo.toml");
        assert!(
            manifest.contains(&format!("tag = \"{}\"", AffectedPlugin::RUFF_TAG)),
            "update AffectedPlugin::RUFF_TAG to the workspace's ruff tag"
        );
    }

    #[test]
    fn modes() {
        assert_eq!(AffectedPlugin::parse_mode(None), Ok(Mode::Off));
        assert_eq!(AffectedPlugin::parse_mode(Some(vec![""])), Ok(Mode::Auto));
        assert_eq!(
            AffectedPlugin::parse_mode(Some(vec!["record"])),
            Ok(Mode::Record)
        );
        assert_eq!(
            AffectedPlugin::parse_mode(Some(vec!["select"])),
            Ok(Mode::Select)
        );
        assert!(AffectedPlugin::parse_mode(Some(vec!["tests/"])).is_err());
    }
}
