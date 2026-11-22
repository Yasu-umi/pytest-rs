//! Affected-test selection (`--affected`, docs/AFFECTED_TESTS.md): run only
//! the tests affected by code that changed since a recorded run.
//!
//! This is stage 1 of the design (§10 "PR 1"): local, single process,
//! selection after import. Without `--affected*` the plugin is inert: no
//! sys.monitoring tool, no data file, no behavior change.

mod collector;
mod config;
mod data;
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

use crate::collector::{AffectedCollector, Recording};
use crate::config::{AffectedConfig, BUILTIN_FULL_RUN_ON, BUILTIN_IGNORE, Globs};
use crate::data::{Data, Dep, FileEntry, Header, SnapEntry, TestEntry};
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

/// Contexts are numbered by the collector; 0 is the global one
/// (`collect:global`, §4.2): plugin and conftest imports, collection hooks
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

    fn start_collector(state: &mut State, py: Python<'_>) -> PyResult<()> {
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
            ),
        )?;
        let py_start = monitoring.getattr("events")?.getattr("PY_START")?;
        monitoring.call_method1(
            "register_callback",
            (tool_id, &py_start, collector.bind(py).getattr("py_start")?),
        )?;
        monitoring.call_method1("set_events", (tool_id, &py_start))?;
        state.collector = Some(collector);
        state.tool_id = tool_id;
        Ok(())
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
            include_py: vec![&builtin_full, &state.config.full_run_on],
            own_files: own,
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

    fn is_explicit(explicit: &[String], nodeid: &str) -> bool {
        explicit.iter().any(|prefix| {
            nodeid == prefix
                || nodeid
                    .strip_prefix(prefix.as_str())
                    .is_some_and(|rest| rest.starts_with("::") || rest.starts_with('['))
        })
    }

    /// The current unit checksums of every file the stored data knows,
    /// parsing only files whose content changed.
    fn current_units(state: &State, data: &Data) -> HashMap<u32, CurrentFile> {
        data.files
            .iter()
            .map(|(&file, entry)| {
                let rel = data.strings.resolve(file);
                if state.config.always_changed.matches(rel) {
                    return (file, CurrentFile::Changed);
                }
                let Ok(bytes) = std::fs::read(state.rootdir.join(rel)) else {
                    return (file, CurrentFile::Changed);
                };
                let content_hash = FileUnits::content_hash(&bytes);
                let units: Option<HashMap<String, u64>> = if content_hash == entry.content_hash {
                    entry.units.as_ref().map(|units| {
                        units
                            .iter()
                            .map(|(q, c)| (data.strings.resolve(*q).to_string(), *c))
                            .collect()
                    })
                } else {
                    std::str::from_utf8(&bytes)
                        .ok()
                        .and_then(FileUnits::parse)
                        .map(|f| f.units)
                };
                match units {
                    Some(units) => (
                        file,
                        CurrentFile::Units {
                            content_hash,
                            units,
                        },
                    ),
                    None => (file, CurrentFile::Changed),
                }
            })
            .collect()
    }

    fn dep_changed(data: &Data, current: &HashMap<u32, CurrentFile>, dep: &Dep) -> bool {
        match current.get(&dep.file) {
            None | Some(CurrentFile::Changed) => true,
            Some(CurrentFile::Units {
                content_hash,
                units,
            }) => {
                let qualname = data.strings.resolve(dep.qualname);
                if qualname == FILE_UNIT {
                    *content_hash != dep.checksum
                } else {
                    units.get(qualname) != Some(&dep.checksum)
                }
            }
        }
    }

    fn select(
        state: &mut State,
        ctx: &mut HookContext,
        items: &mut Vec<TestItem>,
    ) -> Option<String> {
        let Some(data) = state.stored.as_ref() else {
            return Some(format!(
                "running all {} tests ({})",
                items.len(),
                state.unusable.as_deref().unwrap_or("no data")
            ));
        };
        let snapshot = state.snapshot.as_deref().unwrap_or_default();
        let changes = Snapshot::diff(&data.snapshot, snapshot);
        if !changes.paths.is_empty() {
            state.discard_stored = true;
            let shown: Vec<&str> = changes.paths.iter().take(3).map(String::as_str).collect();
            let more = changes.paths.len().saturating_sub(shown.len());
            let more = if more > 0 {
                format!(" and {more} more")
            } else {
                String::new()
            };
            return Some(format!(
                "running all {} tests (changed: {}{more})",
                items.len(),
                shown.join(", ")
            ));
        }
        let current = Self::current_units(state, data);
        let mut group_changed: HashMap<u32, bool> = HashMap::new();
        let explicit = Self::explicit_nodeids(ctx);
        let (mut new, mut failed, mut changed, mut forced) = (0usize, 0usize, 0usize, 0usize);
        let (keep, drop): (Vec<TestItem>, Vec<TestItem>) =
            std::mem::take(items).into_iter().partition(|item| {
                let path = Self::item_path(&state.rootdir, item);
                if Self::is_explicit(&explicit, &item.nodeid)
                    || state.config.always_run.matches(&path)
                {
                    forced += 1;
                    return true;
                }
                let Some(entry) = data.test(&item.nodeid) else {
                    new += 1;
                    return true;
                };
                if entry.failed {
                    failed += 1;
                    return true;
                }
                let affected = entry.groups.iter().any(|group| {
                    *group_changed.entry(*group).or_insert_with(|| {
                        data.group(*group)
                            .iter()
                            .any(|dep| Self::dep_changed(data, &current, dep))
                    })
                });
                if affected {
                    changed += 1;
                }
                affected
            });
        *items = keep;
        let deselected = drop.len();
        ctx.session.deselected_items.extend(drop);
        if items.is_empty() && deselected > 0 {
            ctx.session.no_tests_ok = true;
        }
        let reasons: Vec<String> = [
            (changed, "affected"),
            (new, "new"),
            (failed, "failed"),
            (forced, "forced"),
        ]
        .iter()
        .filter(|(n, _)| *n > 0)
        .map(|(n, what)| format!("{n} {what}"))
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
    /// whether stored entries survive.
    fn apply_mode(
        state: &mut State,
        mode: Mode,
        ctx: &mut HookContext,
        items: &mut Vec<TestItem>,
    ) -> PyResult<()> {
        if mode == Mode::Record {
            // A full-run trigger changed since the stored entries were
            // recorded: tests this run does not cover must not keep them.
            if let Some(data) = &state.stored
                && !Snapshot::diff(
                    &data.snapshot,
                    state.snapshot.as_deref().unwrap_or_default(),
                )
                .paths
                .is_empty()
            {
                state.discard_stored = true;
            }
            return Ok(());
        }
        if mode.selects()
            && let Some(summary) = Self::select(state, ctx, items)
            && !ctx.config.no_terminal()
        {
            println!("affected: {summary}");
        }
        Ok(())
    }

    /// Turn the session's recording into data entries, merged into the
    /// stored data (or a fresh file), and write it.
    fn record(state: &mut State, ctx: &mut HookContext, recording: Recording) -> PyResult<usize> {
        let header = state.header.clone().expect("header computed at configure");
        let mut data = match state.stored.take() {
            Some(data) if !state.discard_stored => data,
            _ => Data::new(header.clone()),
        };
        data.header = header;

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
        for (sid, units, hash) in &file_units {
            let Some(sid) = sid else { continue };
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
        let groups: Vec<Option<u32>> = recording
            .contexts
            .iter()
            .map(|codes| {
                let deps: Vec<Dep> = codes
                    .iter()
                    .flat_map(|code| code_deps[*code as usize].iter().copied())
                    .collect();
                (!deps.is_empty()).then(|| data.add_group(deps))
            })
            .collect();

        // Tests that ran.
        let mut outcomes: HashMap<&str, (bool, f64)> = HashMap::new();
        for report in &ctx.session.reports {
            let entry = outcomes.entry(report.nodeid.as_str()).or_default();
            entry.0 |= report.outcome == Outcome::Failed && !report.rerun;
            entry.1 += report.duration.as_secs_f64();
        }
        let mut recorded = 0usize;
        for nodeid in &state.to_run {
            let Some(&(failed, duration)) = outcomes.get(nodeid.as_str()) else {
                continue;
            };
            // A test reported without reaching the runtest_setup hook (a
            // skip mark) ran none of its own code: its file's import and
            // the global context are its dependencies.
            let (run_path, own, setups) = match state.runs.get(nodeid) {
                Some(run) => (run.path.clone(), Some(run.context), run.setups.clone()),
                None => (
                    nodeid.split("::").next().unwrap_or(nodeid).to_string(),
                    None,
                    BTreeSet::new(),
                ),
            };
            let contexts = std::iter::once(GLOBAL_CONTEXT)
                .chain(state.collect_contexts.get(&run_path).copied())
                .chain(setups)
                .chain(own);
            let test_groups: Vec<u32> = contexts
                .filter_map(|c| groups.get(c).copied().flatten())
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
                },
            );
            recorded += 1;
        }
        // Entries whose test file is gone.
        let rootdir = state.rootdir.clone();
        let gone: Vec<u32> = data
            .tests
            .iter()
            .filter(|(_, entry)| !rootdir.join(data.strings.resolve(entry.path)).exists())
            .map(|(id, _)| *id)
            .collect();
        for id in gone {
            data.tests.remove(&id);
        }
        data.snapshot = match state.snapshot.take() {
            Some(snapshot) => snapshot,
            None => Self::snapshot(state),
        };
        data.save(&state.data_path).map_err(|e| {
            pyo3::exceptions::PyOSError::new_err(format!(
                "cannot write {}: {e}",
                state.data_path.display()
            ))
        })?;
        Ok(recorded)
    }
}

/// A stored file's state now.
enum CurrentFile {
    /// Missing, unreadable, unparseable or configured as always changed:
    /// every unit counts as changed.
    Changed,
    Units {
        content_hash: u64,
        units: HashMap<String, u64>,
    },
}

impl Plugin for AffectedPlugin {
    fn name(&self) -> &str {
        "affected"
    }

    fn pytest_addoption(&self, parser: &mut OptionParser) {
        parser.add_option(OptDef::optional_value(
            "--affected",
            "Run only the tests affected by changes since the recorded run \
             ('auto', the default: select and record what ran; 'record': run \
             as usual and record; 'select': select without recording)",
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
        if mode == Mode::Off || ctx.config.is_worker() {
            return Ok(());
        }
        if ctx
            .config
            .numprocesses_spec()
            .is_some_and(|spec| spec.trim() != "0")
        {
            return Err(pytest_rs_core::python::usage_error(
                py,
                "--affected with -n is not supported yet",
            ));
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
            None if state.data_path.exists() => state.unusable = Some("unreadable data".into()),
            None => state.unusable = Some("no data".into()),
            Some(data) => match data.header.mismatch(&header) {
                Some(reason) => state.unusable = Some(reason.into()),
                None => state.stored = Some(data),
            },
        }
        state.header = Some(header);
        if mode.records() {
            Self::start_collector(&mut state, py)?;
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

    fn pytest_collection_modifyitems(
        &self,
        ctx: &mut HookContext,
        items: &mut Vec<TestItem>,
    ) -> PyResult<()> {
        let mut state = self.lock();
        let Some(mode) = state.mode.filter(|m| *m != Mode::Off) else {
            return Ok(());
        };
        let snapshot = Self::snapshot(&state);
        state.snapshot = Some(snapshot);
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

    fn pytest_sessionfinish(&mut self, ctx: &mut HookContext, _exit_code: i32) -> PyResult<()> {
        let mut state = self.lock();
        let Some(collector) = state.collector.take() else {
            return Ok(());
        };
        let py = ctx.py;
        let monitoring = py.import("sys")?.getattr("monitoring")?;
        monitoring.call_method1("set_events", (state.tool_id, 0))?;
        monitoring.call_method1("free_tool_id", (state.tool_id,))?;
        let recording = collector.borrow(py).take();
        let recorded = Self::record(&mut state, ctx, recording)?;
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

    #[test]
    fn explicit_prefixes() {
        let explicit = vec!["tests/test_a.py::test_x".to_string()];
        assert!(AffectedPlugin::is_explicit(
            &explicit,
            "tests/test_a.py::test_x"
        ));
        assert!(AffectedPlugin::is_explicit(
            &explicit,
            "tests/test_a.py::test_x[1]"
        ));
        assert!(!AffectedPlugin::is_explicit(
            &explicit,
            "tests/test_a.py::test_xy"
        ));
        let class = vec!["tests/test_a.py::TestK".to_string()];
        assert!(AffectedPlugin::is_explicit(
            &class,
            "tests/test_a.py::TestK::test_m"
        ));
    }
}
