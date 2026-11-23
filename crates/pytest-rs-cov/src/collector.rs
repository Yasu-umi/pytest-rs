//! The sys.monitoring callbacks, coverage.py sysmon-style two stages: a
//! global PY_START gate classifies each code object exactly once, arming
//! local LINE (and, in branch mode, BRANCH) events only on tracked code.
//! Untracked code (site-packages, stdlib, the shim) is never
//! line-instrumented, so the per-process first-hit cost — paid again by
//! every fork worker — shrinks from "every line of every dependency" to
//! "one PY_START per code object".

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Arc, Mutex};

use pyo3::prelude::*;

use crate::arcs::{CodeBytes, Opcodes};
use crate::files::{GlobMatcher, PathAliases};

/// filename -> executed arcs in coverage.py's form: (from line, to line), a
/// negative destination leaving the code object (`-co_firstlineno`).
pub type ArcMap = HashMap<String, BTreeSet<(i64, i64)>>;

/// context -> its arcs (`--cov-context`).
pub type ContextArcMap = HashMap<String, ArcMap>;

/// Branch events as recorded: per context, (code key, source offset) ->
/// destination offsets. Resolved to line arcs once, at the end.
type RawBranches = HashMap<Arc<str>, HashMap<(usize, i64), BTreeSet<i64>>>;

struct TrackedCode {
    /// The pin keeps the code object alive so its id is never reused (and
    /// its bytecode can be read when the branch events are resolved).
    code: Py<PyAny>,
    filename: Arc<str>,
}

type TrackedCodes = Mutex<HashMap<usize, TrackedCode>>;

#[pyclass]
pub struct LineCollector {
    /// rootdir with a trailing separator, for prefix matching.
    rootdir: String,
    /// Source filters (absolute, dirs end with a separator); empty = any
    /// file under rootdir.
    sources: Vec<String>,
    /// coverage `[paths]` aliases: a file whose path prefix-matches an alias
    /// is measured (e.g. a script rsync'd to a worker dir under `*/dir1`),
    /// then remapped to canonical at report time.
    aliases: PathAliases,
    /// `[run] omit`: never measured.
    omit: GlobMatcher,
    /// `[run] include` (only without a source): measured if matched.
    include: GlobMatcher,
    /// File name -> what omit / include decided (None: neither applies).
    filtered: Mutex<HashMap<String, Option<bool>>>,
    /// The embedded shim dir; never measured.
    shim_root: String,
    hits: Mutex<HashMap<String, BTreeSet<u32>>>,
    /// Branch mode: the branch events, per context ("" without
    /// `--cov-context`).
    branches: Mutex<RawBranches>,
    no_context: Arc<str>,
    /// `--cov-context=test`: every hit line is ALSO recorded per (context,
    /// file), in addition to the blanket `hits` map used for the coverage
    /// %/table report. `current_context` is updated by CovPlugin's
    /// runtest_setup/pyfunc_call/runtest_teardown hooks (coverage.py's
    /// TestContextPlugin switches at exactly those three boundaries).
    context_hits: Mutex<HashMap<String, HashMap<String, BTreeSet<u32>>>>,
    current_context: Mutex<Arc<str>>,
    track_contexts: bool,
    tracked_codes: TrackedCodes,
    /// The LINE (| BRANCH...) event mask armed per tracked code object.
    local_events: i64,
    disable: Py<PyAny>,
    monitoring: Py<PyAny>,
    tool_id: u8,
}

impl LineCollector {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        rootdir: String,
        sources: Vec<String>,
        aliases: PathAliases,
        omit: GlobMatcher,
        include: GlobMatcher,
        shim_root: String,
        local_events: i64,
        disable: Py<PyAny>,
        monitoring: Py<PyAny>,
        tool_id: u8,
        track_contexts: bool,
    ) -> Self {
        Self {
            rootdir,
            sources,
            aliases,
            omit,
            include,
            filtered: Mutex::new(HashMap::new()),
            shim_root,
            hits: Mutex::new(HashMap::new()),
            branches: Mutex::new(HashMap::new()),
            no_context: Arc::from(""),
            context_hits: Mutex::new(HashMap::new()),
            current_context: Mutex::new(Arc::from("")),
            track_contexts,
            tracked_codes: Mutex::new(HashMap::new()),
            local_events,
            disable,
            monitoring,
            tool_id,
        }
    }

    pub fn take_hits(&self) -> HashMap<String, BTreeSet<u32>> {
        std::mem::take(&mut self.hits.lock().expect("collector lock poisoned"))
    }

    /// The branch events as line arcs: all of them, and per context with
    /// `--cov-context` (empty otherwise). `multiline(file)` is the file's
    /// continuation line -> statement first line map.
    pub fn take_arcs(
        &self,
        py: Python<'_>,
        multiline: &mut dyn FnMut(&str) -> BTreeMap<u32, u32>,
    ) -> PyResult<(ArcMap, ContextArcMap)> {
        let raw = std::mem::take(&mut *self.branches.lock().expect("collector lock poisoned"));
        let mut arcs = ArcMap::new();
        let mut by_context = ContextArcMap::new();
        if raw.is_empty() {
            return Ok((arcs, by_context));
        }
        let ops = Opcodes::load(py)?;
        let codes: HashMap<usize, (Py<PyAny>, Arc<str>)> = self
            .tracked_codes
            .lock()
            .expect("collector lock poisoned")
            .iter()
            .map(|(key, entry)| {
                (
                    *key,
                    (entry.code.clone_ref(py), Arc::clone(&entry.filename)),
                )
            })
            .collect();
        let mut bytes: HashMap<usize, Option<CodeBytes>> = HashMap::new();
        let mut maps: HashMap<Arc<str>, BTreeMap<u32, u32>> = HashMap::new();
        for (context, sites) in raw {
            for ((key, source), dests) in sites {
                let Some((code, filename)) = codes.get(&key) else {
                    continue;
                };
                let code_bytes = bytes
                    .entry(key)
                    .or_insert_with(|| CodeBytes::read(code.bind(py)).ok());
                let Some(code_bytes) = code_bytes else {
                    continue;
                };
                let map = maps
                    .entry(Arc::clone(filename))
                    .or_insert_with(|| multiline(filename));
                for dest in dests {
                    let Some(arc) = code_bytes.resolve(&ops, map, source, dest) else {
                        continue;
                    };
                    arcs.entry(filename.to_string()).or_default().insert(arc);
                    if self.track_contexts {
                        by_context
                            .entry(context.to_string())
                            .or_default()
                            .entry(filename.to_string())
                            .or_default()
                            .insert(arc);
                    }
                }
            }
        }
        Ok((arcs, by_context))
    }

    pub fn take_context_hits(&self) -> HashMap<String, HashMap<String, BTreeSet<u32>>> {
        std::mem::take(&mut self.context_hits.lock().expect("collector lock poisoned"))
    }

    /// Switches the "current" context: subsequent hits attribute to it
    /// until the next switch. A no-op when `--cov-context` isn't active.
    ///
    /// Lines return DISABLE after their first hit in a context, so the
    /// switch re-arms them (`restart_events`): each line then fires once
    /// per context that runs it, instead of on every execution.
    pub fn set_context(&self, py: Python<'_>, context: &str) -> PyResult<()> {
        if self.track_contexts {
            *self
                .current_context
                .lock()
                .expect("collector lock poisoned") = Arc::from(context);
            self.monitoring.bind(py).call_method0("restart_events")?;
        }
        Ok(())
    }

    /// Lexically drop `.` components (`a/./b` -> `a/b`), as coverage.py's
    /// `abs_file` does through `os.path.normpath`.
    fn normalize(filename: &str) -> String {
        if !filename.contains("/./") && !filename.contains("\\.\\") {
            return filename.to_string();
        }
        std::path::Path::new(filename)
            .components()
            .collect::<std::path::PathBuf>()
            .to_string_lossy()
            .into_owned()
    }

    fn tracked(&self, filename: &str) -> bool {
        if filename.starts_with('<')
            || filename.starts_with(&self.shim_root)
            || filename.contains("site-packages")
            || filename.contains("/lib/python")
        {
            return false;
        }
        if !self.omit.is_empty() || !self.include.is_empty() {
            let mut decided = self.filtered.lock().expect("collector lock poisoned");
            let verdict = *decided.entry(filename.to_string()).or_insert_with(|| {
                let path = std::path::Path::new(filename);
                if self.omit.matches_path(path) {
                    Some(false)
                } else if !self.include.is_empty() {
                    // [run] include (only without a source): it alone decides.
                    Some(self.include.matches_path(path))
                } else {
                    None
                }
            });
            if let Some(verdict) = verdict {
                return verdict;
            }
        }
        if self.sources.is_empty() {
            if filename.starts_with(&self.rootdir) {
                return true;
            }
        } else if self.sources.iter().any(|source| {
            filename == source.trim_end_matches('/') || filename.starts_with(source.as_str())
        }) {
            return true;
        }
        // coverage `[paths]` aliases: a file rsync'd to a worker dir
        // (matched by an alias like `*/dir1`) is measured even though it is
        // not under the `--cov` source root; the recorded alias path is then
        // remapped to canonical at report time.
        self.aliases.matches(filename)
    }

    /// Record one branch event under the current context; returns how many
    /// destinations its source has had there.
    fn record_branch(&self, code_key: usize, src_offset: i64, dst_offset: i64) -> usize {
        let context = if self.track_contexts {
            Arc::clone(
                &self
                    .current_context
                    .lock()
                    .expect("collector lock poisoned"),
            )
        } else {
            Arc::clone(&self.no_context)
        };
        let mut branches = self.branches.lock().expect("collector lock poisoned");
        let dests = branches
            .entry(context)
            .or_default()
            .entry((code_key, src_offset))
            .or_default();
        dests.insert(dst_offset);
        dests.len()
    }

    /// [paths] remap, applied once per code object (`py_start`) so every hit
    /// and arc is stored under the canonical name from the start — letting
    /// workers whose chdir'd copies land under different alias prefixes
    /// (`*/dir1`, `*/dir2`) merge into a single row instead of surviving as
    /// separate ones that only happen to print the same name.
    fn canonical_name(&self, filename: &str) -> Arc<str> {
        Arc::from(
            self.aliases
                .map(filename)
                .unwrap_or_else(|| filename.to_string()),
        )
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
            // `sys.path` entries like `rootdir/.` (pytest's `pythonpath =
            // ["."]`) leave `/./` in file names; coverage.py normalizes them
            // before matching sources.
            let filename = Self::normalize(&filename);
            // Like coverage.py: synthesized annotation scopes are not
            // user code.
            let name: String = code.getattr("co_name")?.extract()?;
            if name != "__annotate__" && self.tracked(&filename) {
                self.monitoring
                    .bind(py)
                    .call_method1("set_local_events", (self.tool_id, &code, self.local_events))?;
                self.tracked_codes
                    .lock()
                    .expect("collector lock poisoned")
                    .insert(
                        key,
                        TrackedCode {
                            code: code.unbind(),
                            filename: self.canonical_name(&filename),
                        },
                    );
            }
        }
        Ok(self.disable.clone_ref(py))
    }

    /// Local LINE on tracked code: record the hit and DISABLE, so every
    /// tracked line costs exactly one callback ever. Under `--cov-context`
    /// a later execution in another context (the next parametrized case)
    /// must still be recorded: `set_context` re-arms every disabled event
    /// at each switch, so a line costs one callback per context that runs
    /// it. (coverage.py's sys.monitoring core does not support dynamic
    /// contexts and falls back to its settrace tracer, which sees every
    /// execution.)
    fn line(&self, py: Python<'_>, code: Bound<'_, PyAny>, line: u32) -> PyResult<Py<PyAny>> {
        let key = code.as_ptr() as usize;
        let filename = self
            .tracked_codes
            .lock()
            .expect("collector lock poisoned")
            .get(&key)
            .map(|entry| Arc::clone(&entry.filename));
        if let Some(filename) = filename {
            self.hits
                .lock()
                .expect("collector lock poisoned")
                .entry(filename.as_ref().to_string())
                .or_default()
                .insert(line);
            if self.track_contexts {
                let context = self
                    .current_context
                    .lock()
                    .expect("collector lock poisoned");
                self.context_hits
                    .lock()
                    .expect("collector lock poisoned")
                    .entry(context.to_string())
                    .or_default()
                    .entry(filename.as_ref().to_string())
                    .or_default()
                    .insert(line);
                drop(context);
            }
        }
        Ok(self.disable.clone_ref(py))
    }

    /// 3.14 BRANCH_LEFT / BRANCH_RIGHT: each direction fires once per
    /// branch site (DISABLE is per direction there).
    fn branch_left(
        &self,
        py: Python<'_>,
        code: Bound<'_, PyAny>,
        src_offset: i64,
        dst_offset: i64,
    ) -> PyResult<Py<PyAny>> {
        self.record_branch(code.as_ptr() as usize, src_offset, dst_offset);
        Ok(self.disable.clone_ref(py))
    }

    fn branch_right(
        &self,
        py: Python<'_>,
        code: Bound<'_, PyAny>,
        src_offset: i64,
        dst_offset: i64,
    ) -> PyResult<Py<PyAny>> {
        self.record_branch(code.as_ptr() as usize, src_offset, dst_offset);
        Ok(self.disable.clone_ref(py))
    }

    /// 3.13 single BRANCH event: DISABLE would silence both directions, so
    /// it is only returned once both destinations have been observed (in
    /// this context: a context switch re-arms every event). A site that only
    /// ever resolves one way stays armed and fires on every execution, so
    /// the repeat costs one lock and one set probe.
    fn branch_compat(
        &self,
        py: Python<'_>,
        code: Bound<'_, PyAny>,
        src_offset: i64,
        dst_offset: i64,
    ) -> PyResult<Py<PyAny>> {
        if self.record_branch(code.as_ptr() as usize, src_offset, dst_offset) >= 2 {
            Ok(self.disable.clone_ref(py))
        } else {
            Ok(py.None())
        }
    }
}
