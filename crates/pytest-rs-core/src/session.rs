use std::any::{Any, TypeId};
use std::collections::HashMap;

use pyo3::prelude::*;

use crate::collect::TestItem;
use crate::fixture::{FixtureRegistry, Scope};
use crate::report::TestReport;

/// Key identifying one live instance of a fixture value in the cache:
/// (fixture name, scope instance, fixture param index). The scope instance
/// is "" for session scope, the module nodeid for module scope, and the
/// item nodeid for function scope.
pub type CacheKey = (String, String, Option<usize>);

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
    /// A plugin may force the session exit code (e.g. --cov-fail-under).
    pub exit_code_override: Option<i32>,
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
            exit_code_override: None,
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
