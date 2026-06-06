use std::any::{Any, TypeId};
use std::collections::HashMap;

use pyo3::prelude::*;

use crate::collect::TestItem;
use crate::fixture::{FixtureRegistry, Scope};
use crate::report::TestReport;

/// Key identifying one live instance of a fixture value in the cache:
/// (fixture name, definition baseid, scope instance, fixture param index).
/// The baseid distinguishes override levels of the same name; the scope
/// instance is "" for session scope, the module nodeid for module scope,
/// and the item nodeid for function scope.
pub type CacheKey = (String, String, String, Option<usize>);

/// Teardown work registered during fixture setup, run LIFO when the owning
/// scope finishes.
pub enum Finalizer {
    /// A Python callable invoked with no arguments.
    Callable(Py<PyAny>),
    /// A suspended sync generator fixture; teardown resumes it once and
    /// expects StopIteration.
    GenNext(Py<PyAny>),
}

pub struct PendingFinalizer {
    pub scope: Scope,
    pub instance: String,
    pub finalizer: Finalizer,
}

/// A pytest_* hook function defined in a conftest.py.
pub struct PyHook {
    pub name: String,
    pub func: Py<PyAny>,
    /// Visibility prefix (the conftest's directory), "" for rootdir.
    pub baseid: String,
    /// The `pytest_plugins` module this hook came from, if any (used to
    /// avoid re-registering a plugin declared by several test modules).
    pub plugin_module: Option<String>,
}

/// Mutable state shared by the engine and every hook for one test run.
pub struct Session {
    pub items: Vec<TestItem>,
    pub registry: FixtureRegistry,
    /// Cached fixture values, GIL-independent handles.
    pub fixture_cache: HashMap<CacheKey, Py<PyAny>>,
    /// LIFO stack of pending finalizers across all scopes.
    pub finalizers: Vec<PendingFinalizer>,
    pub reports: Vec<TestReport>,
    /// Plugin scratch space, mirroring pytest's config/session stash.
    stash: HashMap<TypeId, Box<dyn Any>>,
    /// Well-known shared Python objects published by plugins
    /// (e.g. "asyncio.event_loop").
    pub py_stash: HashMap<String, Py<PyAny>>,
    /// pytest_* hook functions collected from conftest.py files,
    /// registration order (rootdir first).
    pub py_hooks: Vec<PyHook>,
    /// A plugin may force the session exit code (e.g. --cov-fail-under).
    pub exit_code_override: Option<i32>,
    /// "!!! ... !!!" banner for pytest.exit / Ctrl-C aborts.
    pub abort_banner: Option<String>,
    /// Warnings forwarded from -n workers, merged into the summary.
    pub worker_warnings: Vec<String>,
    pub worker_warning_count: usize,
    /// Fatal distribution condition (crashed-worker budget exhausted),
    /// shown as a banner before the short summary.
    pub dist_banner: Option<String>,
    /// log_cli live logging: the runner prints per-item headers and
    /// word-style outcomes so log records interleave with progress.
    pub live_logging: bool,
    /// (done, total) for the current item's live outcome line.
    pub live_progress: Option<(usize, usize)>,
    /// How many of the current item's reports already printed live.
    pub live_printed: usize,
    /// Items dropped by selection (-k/-m/--lf), for the summary line.
    pub deselected: usize,
    /// The dropped items themselves, passed to pytest_deselected hooks.
    pub deselected_items: Vec<TestItem>,
    /// Collection errors as (nodeid, longrepr), shown in the ERRORS section
    /// (and excluded from FAILURES / " - msg" summary suffixes).
    pub collect_errors: Vec<(String, String)>,
    /// Set when --maxfail/-x stopped the run, with the failure count, for
    /// the "stopping after N failures" banner.
    pub stopped_after: Option<usize>,
}

impl Session {
    pub fn new() -> Self {
        Self {
            items: Vec::new(),
            registry: FixtureRegistry::default(),
            fixture_cache: HashMap::new(),
            finalizers: Vec::new(),
            reports: Vec::new(),
            stash: HashMap::new(),
            py_stash: HashMap::new(),
            py_hooks: Vec::new(),
            exit_code_override: None,
            abort_banner: None,
            worker_warnings: Vec::new(),
            worker_warning_count: 0,
            dist_banner: None,
            live_logging: false,
            live_progress: None,
            live_printed: 0,
            deselected: 0,
            deselected_items: Vec::new(),
            collect_errors: Vec::new(),
            stopped_after: None,
        }
    }

    pub fn stash_insert<T: Any>(&mut self, value: T) {
        self.stash.insert(TypeId::of::<T>(), Box::new(value));
    }

    pub fn stash_get<T: Any>(&self) -> Option<&T> {
        self.stash
            .get(&TypeId::of::<T>())
            .and_then(|b| b.downcast_ref())
    }

    pub fn stash_get_mut<T: Any>(&mut self) -> Option<&mut T> {
        self.stash
            .get_mut(&TypeId::of::<T>())
            .and_then(|b| b.downcast_mut())
    }
}

impl Default for Session {
    fn default() -> Self {
        Self::new()
    }
}
