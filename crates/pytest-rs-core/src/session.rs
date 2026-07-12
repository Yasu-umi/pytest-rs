use std::any::{Any, TypeId};
use std::collections::HashMap;

use pyo3::prelude::*;

use crate::collect::TestItem;
use crate::fixture::{FixtureRegistry, Scope};
use crate::report::TestReport;

/// Key identifying one live instance of a fixture value in the cache:
/// (scope, fixture name, definition baseid, scope instance, fixture param index).
/// The baseid distinguishes override levels of the same name; the scope
/// instance is "" for session scope, the module nodeid for module scope,
/// and the item nodeid for function scope. The scope is part of the key
/// because instance strings collide across scopes (a module-level test has
/// the same class instance — the file — as its module instance), so scope
/// teardown must evict only the matching scope.
pub type CacheKey = (Scope, String, String, String, Option<String>);

/// One non-function-scope parametrization a fixture's value transitively
/// depends on: (param scope, the scope-instance the param is constant within,
/// argname, the param value's repr). When such a param's value changes while
/// its scope-instance stays the same (e.g. a class-scoped `params=` fixture
/// moving to its next value within the same class node), every fixture
/// carrying that binding must be torn down before the next value is set up —
/// pytest's `FixtureDef.execute` finishing a differently-parametrized cached
/// instance. The value repr (not the param index) is the key so two functions
/// parametrizing the same fixture with overlapping values at different indices
/// still reuse the cached instance (e.g. issue634).
pub type Binding = (Scope, String, String, String);

/// A cached fixture outcome plus the parametrization bindings it depends on
/// (used to evict it on a mid-node param transition). pytest caches a fixture's
/// raised exception alongside its value, so a setup that fails is not re-run for
/// sibling items in the same scope — the cached exception is re-raised instead.
pub struct CachedFixture {
    /// The fixture value (`None` placeholder when setup raised).
    pub value: Py<PyAny>,
    /// The exception the fixture raised during setup, re-raised on cache hit.
    pub error: Option<Py<PyAny>>,
    /// The traceback captured when `error` was raised; restored on each
    /// re-raise so the cached traceback does not grow per sibling item (#12204).
    pub error_tb: Option<Py<PyAny>>,
    pub bindings: Vec<Binding>,
}

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
    /// Parametrization bindings this finalizer's fixture depends on; lets a
    /// mid-node param transition run it (LIFO) ahead of the deferred scope
    /// teardown. Empty for finalizers that don't depend on any higher-scope
    /// parametrization.
    pub bindings: Vec<Binding>,
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
    /// @pytest.hookimpl(trylast=True): fires after non-trylast hooks.
    pub trylast: bool,
    /// @pytest.hookimpl(tryfirst=True): fires before non-tryfirst hooks.
    pub tryfirst: bool,
}

/// Mutable state shared by the engine and every hook for one test run.
pub struct Session {
    pub items: Vec<TestItem>,
    pub registry: FixtureRegistry,
    /// Cached fixture values, GIL-independent handles.
    pub fixture_cache: HashMap<CacheKey, CachedFixture>,
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
    /// A KeyboardInterrupt during a test's setup/call: (short crash line,
    /// full traceback). Rendered as its own "!!! KeyboardInterrupt !!!"
    /// block after the whole summary (upstream's `_report_keyboardinterrupt`),
    /// not as `abort_banner` (which prints before the summary, for
    /// pytest.exit's different, immediate-abort banner).
    pub keyboard_interrupt_repr: Option<(String, String)>,
    /// Warnings forwarded from -n workers, merged into the summary.
    pub worker_warnings: Vec<String>,
    pub worker_warning_count: usize,
    /// Which -n worker produced each failed report (nodeid -> gw index),
    /// for the "[gw0] darwin -- Python ..." line atop each failure repr.
    pub report_workers: HashMap<String, usize>,
    /// The shared "darwin -- Python 3.13.2 /usr/bin/python" suffix of that
    /// line (upstream getworkerinfoline; our workers share the interpreter).
    pub worker_platinfo: Option<String>,
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
    /// --setup-show: reports whose progress char already streamed mid-item
    /// (between the item line and the TEARDOWN narration).
    pub streamed_chars: usize,
    /// Items dropped by selection (-k/-m/--lf), for the summary line.
    pub deselected: usize,
    /// The dropped items themselves, passed to pytest_deselected hooks.
    pub deselected_items: Vec<TestItem>,
    /// Collection errors as (nodeid, longrepr), shown in the ERRORS section
    /// (and excluded from FAILURES / " - msg" summary suffixes).
    pub collect_errors: Vec<(String, String)>,
    /// Explicit node-id args (`file::name`) that matched nothing after
    /// collection. A non-empty set forces USAGE_ERROR (exit 4) even when
    /// collection errors aborted the session, mirroring upstream (#134).
    pub not_found_nodeids: Vec<String>,
    /// Module nodeids skipped at collection time (pytest.skip(allow_module_level=True),
    /// --doctest-ignore-import-errors, etc.), for collector-tree hook synthesis.
    /// Each entry is (nodeid, reason, location) where location is "file:line".
    pub skipped_modules: Vec<(String, String, String)>,
    /// Files skipped by a conftest/plugin `pytest_collect_file` hook that raised
    /// `pytest.skip()`. Their parent Dir collectreport should be "skipped".
    /// Each entry is (nodeid, reason).
    pub collect_file_skips: Vec<(String, String)>,
    /// Set when --maxfail/-x stopped the run, with the failure count, for
    /// the "stopping after N failures" banner.
    pub stopped_after: Option<usize>,
    /// Plugin-set session.shouldfail message (e.g. pytest-timeout's
    /// session deadline): aborts the run with a "!!! msg !!!" banner.
    pub shouldfail: Option<String>,
    /// The python plugin object that replaced the 'terminalreporter'
    /// plugin (pytest-sugar/pytest-pretty); the engine drives it through
    /// reporter hook calls instead of rendering natively.
    pub custom_reporter: Option<Py<PyAny>>,
    /// "name-version" strings for autoloaded third-party plugins (pytest11
    /// entry points), for the session header's "plugins:" line. Empty when
    /// no dist-backed plugins loaded, in which case the line is omitted.
    pub plugin_distinfo: Vec<String>,
    /// Set for the current item when a plain pytest_runtest_protocol plugin
    /// handled it: the shim TerminalReporter already rendered its reports, so
    /// run_items counts them without re-rendering.
    pub delegated_render: bool,
    /// Total items collected across all workers in dist (-n) mode, set once
    /// the merge loop receives every worker's Collection. `self.items` stays
    /// empty in dist mode (the controller never collects locally), so this
    /// is the only way finish_session can tell a zero-item dist run apart
    /// from an ordinary all-passed one to return NO_TESTS_COLLECTED.
    pub dist_total_items: Option<usize>,
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
            keyboard_interrupt_repr: None,
            worker_warnings: Vec::new(),
            worker_warning_count: 0,
            report_workers: HashMap::new(),
            worker_platinfo: None,
            dist_banner: None,
            live_logging: false,
            live_progress: None,
            live_printed: 0,
            streamed_chars: 0,
            deselected: 0,
            deselected_items: Vec::new(),
            collect_errors: Vec::new(),
            not_found_nodeids: Vec::new(),
            skipped_modules: Vec::new(),
            collect_file_skips: Vec::new(),
            stopped_after: None,
            shouldfail: None,
            custom_reporter: None,
            plugin_distinfo: Vec::new(),
            delegated_render: false,
            dist_total_items: None,
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
