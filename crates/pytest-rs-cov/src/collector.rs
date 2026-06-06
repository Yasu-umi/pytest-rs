//! The sys.monitoring callbacks, coverage.py sysmon-style two stages: a
//! global PY_START gate classifies each code object exactly once, arming
//! local LINE (and, in branch mode, BRANCH) events only on tracked code.
//! Untracked code (site-packages, stdlib, the shim) is never
//! line-instrumented, so the per-process first-hit cost — paid again by
//! every fork worker — shrinks from "every line of every dependency" to
//! "one PY_START per code object".

use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, Mutex};

use pyo3::prelude::*;

/// filename -> executed (source line, destination line, direction) arcs.
pub type ArcMap = HashMap<String, BTreeSet<(u32, i64, u8)>>;

struct TrackedCode {
    /// The pin keeps the code object alive so its id is never reused.
    _code: Py<PyAny>,
    filename: Arc<str>,
    /// co_lines() as sorted (start, end, line) ranges (line -1 when None),
    /// for offset -> line lookups; built only in branch mode.
    lines: Option<Arc<Vec<(i64, i64, i64)>>>,
    /// 3.13 only: conditional-jump targets by instruction offset (from
    /// dis), to classify BRANCH event directions.
    jump_targets: Option<Arc<HashMap<i64, i64>>>,
}

type TrackedCodes = Mutex<HashMap<usize, TrackedCode>>;

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
    /// Branch mode: executed (source line, destination line, direction)
    /// arcs; destination -1 = "no line", direction 1 = fall-through (LEFT),
    /// 2 = jump (RIGHT), 0 = unknown (3.13 without dis info).
    arcs: Mutex<ArcMap>,
    /// 3.13 single-BRANCH-event fallback: destinations seen per
    /// (code id, instruction offset). DISABLE there kills both directions,
    /// so it is only returned once both have been observed.
    seen_dests: Mutex<HashMap<(usize, i64), BTreeSet<i64>>>,
    tracked_codes: TrackedCodes,
    branch: bool,
    /// 3.13: BRANCH events carry no direction; jump targets disassembled
    /// per code object classify them.
    need_jump_targets: bool,
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
        shim_root: String,
        branch: bool,
        need_jump_targets: bool,
        local_events: i64,
        disable: Py<PyAny>,
        monitoring: Py<PyAny>,
        tool_id: u8,
    ) -> Self {
        Self {
            rootdir,
            sources,
            shim_root,
            hits: Mutex::new(HashMap::new()),
            arcs: Mutex::new(HashMap::new()),
            seen_dests: Mutex::new(HashMap::new()),
            tracked_codes: Mutex::new(HashMap::new()),
            branch,
            need_jump_targets,
            local_events,
            disable,
            monitoring,
            tool_id,
        }
    }

    pub fn take_hits(&self) -> HashMap<String, BTreeSet<u32>> {
        std::mem::take(&mut self.hits.lock().expect("collector lock poisoned"))
    }

    pub fn take_arcs(&self) -> ArcMap {
        std::mem::take(&mut self.arcs.lock().expect("collector lock poisoned"))
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

    /// Record one executed (source line, destination line, direction) arc.
    fn record_arc(&self, code_key: usize, src_offset: i64, dst_offset: i64, direction: u8) {
        let tracked = self.tracked_codes.lock().expect("collector lock poisoned");
        let Some(entry) = tracked.get(&code_key) else {
            return;
        };
        let Some(lines) = entry.lines.as_ref().map(Arc::clone) else {
            return;
        };
        // 3.13 fallback: classify the direction from the disassembled jump
        // target. The event's destination can land past the target
        // (FOR_ITER exhaustion skips the loop cleanup), so a forward jump
        // is "at or beyond the target".
        let direction = if direction == 0 {
            match entry.jump_targets.as_ref().and_then(|t| t.get(&src_offset)) {
                Some(target)
                    if (*target >= src_offset && dst_offset >= *target)
                        || (*target < src_offset && dst_offset == *target) =>
                {
                    2
                }
                Some(_) => 1,
                None => 0,
            }
        } else {
            direction
        };
        let filename = entry.filename.as_ref().to_string();
        drop(tracked);
        let src = line_at(&lines, src_offset);
        if src <= 0 {
            return;
        }
        let dst = line_at(&lines, dst_offset);
        self.arcs
            .lock()
            .expect("collector lock poisoned")
            .entry(filename)
            .or_default()
            .insert((src as u32, dst, direction));
    }
}

/// Conditional-jump targets by instruction offset, via dis (3.13, where
/// BRANCH events do not tell which direction resolved).
fn conditional_jump_targets(
    py: Python<'_>,
    code: &Bound<'_, PyAny>,
) -> PyResult<HashMap<i64, i64>> {
    const CONDITIONAL: [&str; 5] = [
        "POP_JUMP_IF_FALSE",
        "POP_JUMP_IF_TRUE",
        "POP_JUMP_IF_NONE",
        "POP_JUMP_IF_NOT_NONE",
        "FOR_ITER",
    ];
    let mut targets = HashMap::new();
    let dis = py.import("dis")?;
    for instruction in dis.call_method1("get_instructions", (code,))?.try_iter()? {
        let instruction = instruction?;
        let opname: String = instruction.getattr("opname")?.extract()?;
        if CONDITIONAL.contains(&opname.as_str())
            && let Ok(Some(target)) = instruction.getattr("argval")?.extract::<Option<i64>>()
        {
            let offset: i64 = instruction.getattr("offset")?.extract()?;
            targets.insert(offset, target);
        }
    }
    Ok(targets)
}

/// The line for a bytecode offset, from sorted co_lines() ranges (-1 when
/// the offset has no line).
fn line_at(lines: &[(i64, i64, i64)], offset: i64) -> i64 {
    lines
        .iter()
        .find(|(start, end, _)| *start <= offset && offset < *end)
        .map(|(_, _, line)| *line)
        .unwrap_or(-1)
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
                self.monitoring
                    .bind(py)
                    .call_method1("set_local_events", (self.tool_id, &code, self.local_events))?;
                let lines = if self.branch {
                    let mut table: Vec<(i64, i64, i64)> = Vec::new();
                    for entry in code.call_method0("co_lines")?.try_iter()? {
                        let (start, end, line): (i64, i64, Option<i64>) = entry?.extract()?;
                        table.push((start, end, line.unwrap_or(-1)));
                    }
                    table.sort_unstable();
                    Some(Arc::new(table))
                } else {
                    None
                };
                let jump_targets = if self.branch && self.need_jump_targets {
                    Some(Arc::new(conditional_jump_targets(py, &code)?))
                } else {
                    None
                };
                self.tracked_codes
                    .lock()
                    .expect("collector lock poisoned")
                    .insert(
                        key,
                        TrackedCode {
                            _code: code.unbind(),
                            filename: Arc::from(filename),
                            lines,
                            jump_targets,
                        },
                    );
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
            .map(|entry| Arc::clone(&entry.filename));
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

    /// 3.14 BRANCH_LEFT / BRANCH_RIGHT: each direction fires once per
    /// branch site (DISABLE is per direction there).
    fn branch_left(
        &self,
        py: Python<'_>,
        code: Bound<'_, PyAny>,
        src_offset: i64,
        dst_offset: i64,
    ) -> PyResult<Py<PyAny>> {
        self.record_arc(code.as_ptr() as usize, src_offset, dst_offset, 1);
        Ok(self.disable.clone_ref(py))
    }

    fn branch_right(
        &self,
        py: Python<'_>,
        code: Bound<'_, PyAny>,
        src_offset: i64,
        dst_offset: i64,
    ) -> PyResult<Py<PyAny>> {
        self.record_arc(code.as_ptr() as usize, src_offset, dst_offset, 2);
        Ok(self.disable.clone_ref(py))
    }

    /// 3.13 single BRANCH event: DISABLE would silence both directions, so
    /// it is only returned once both destinations have been observed.
    fn branch_compat(
        &self,
        py: Python<'_>,
        code: Bound<'_, PyAny>,
        src_offset: i64,
        dst_offset: i64,
    ) -> PyResult<Py<PyAny>> {
        let key = code.as_ptr() as usize;
        self.record_arc(key, src_offset, dst_offset, 0);
        let mut seen = self.seen_dests.lock().expect("collector lock poisoned");
        let dests = seen.entry((key, src_offset)).or_default();
        dests.insert(dst_offset);
        if dests.len() >= 2 {
            Ok(self.disable.clone_ref(py))
        } else {
            Ok(py.None())
        }
    }
}
