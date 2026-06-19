use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use pyo3::prelude::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Scope {
    Function,
    Class,
    Module,
    Package,
    Session,
}

impl Scope {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "function" => Some(Self::Function),
            "class" => Some(Self::Class),
            "module" => Some(Self::Module),
            "package" => Some(Self::Package),
            "session" => Some(Self::Session),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Function => "function",
            Self::Class => "class",
            Self::Module => "module",
            Self::Package => "package",
            Self::Session => "session",
        }
    }
}

/// One `@pytest.fixture` definition discovered at collection time, or
/// registered by a plugin.
pub struct FixtureDef {
    pub name: String,
    /// The underlying Python callable (GIL-independent handle).
    pub func: Py<PyAny>,
    pub scope: Scope,
    pub autouse: bool,
    pub is_coroutine: bool,
    pub is_generator: bool,
    pub is_async_gen: bool,
    /// Names of the fixtures this fixture itself requests.
    pub param_names: Vec<String>,
    /// Visibility prefix: items whose nodeid starts with this see the
    /// fixture. "" = global (plugin / rootdir conftest).
    pub baseid: String,
    /// Defined inside a Test* class: call with the test instance as `self`.
    pub needs_instance: bool,
    /// @pytest.fixture(params=[...]) values; items using this fixture are
    /// expanded per param at collection time.
    pub params: Option<Py<PyAny>>,
    /// @pytest.fixture(ids=...): a list of ids or a callable deriving one
    /// per param value (nodeid suffixes and --setup-show display).
    pub ids: Option<Py<PyAny>>,
    /// Set when the declared scope is not a valid scope name (e.g.
    /// `scope="functions"`): the pytest message to fail with when the fixture
    /// is requested. pytest raises this from `Scope.from_user` at FixtureDef
    /// construction; we defer it to resolution so collection still proceeds.
    pub scope_error: Option<String>,
}

/// All fixture definitions visible in this session, name -> defs ordered
/// from most general (plugins, root conftest) to most specific (test module).
/// Lookup walks in reverse so the most specific visible definition wins.
#[derive(Default)]
pub struct FixtureRegistry {
    by_name: HashMap<String, Vec<Arc<FixtureDef>>>,
}

impl FixtureRegistry {
    pub fn register(&mut self, def: FixtureDef) {
        self.by_name
            .entry(def.name.clone())
            .or_default()
            .push(Arc::new(def));
    }

    /// Every registered definition (all names, all overrides), for building
    /// the pytest-bdd FixtureManager._arg2fixturedefs view.
    pub fn all_defs(&self) -> impl Iterator<Item = &Arc<FixtureDef>> {
        self.by_name.values().flatten()
    }

    /// The most specific definition of `name` visible from `nodeid`.
    pub fn lookup(&self, name: &str, nodeid: &str) -> Option<Arc<FixtureDef>> {
        self.by_name.get(name).and_then(|defs| {
            defs.iter()
                .rev()
                .find(|def| nodeid.starts_with(&def.baseid))
                .cloned()
        })
    }

    /// Fixture override: the next less-specific visible definition of
    /// `name`, below `current` (a fixture requesting its own name).
    pub fn lookup_overridden(
        &self,
        name: &str,
        nodeid: &str,
        current: &Arc<FixtureDef>,
    ) -> Option<Arc<FixtureDef>> {
        self.by_name.get(name).and_then(|defs| {
            defs.iter()
                .rev()
                .skip_while(|def| !Arc::ptr_eq(def, current))
                .skip(1)
                .find(|def| nodeid.starts_with(&def.baseid))
                .cloned()
        })
    }

    /// The transitive fixture closure for an item, in pytest's
    /// getfixtureclosure order: autouse + requested names seed the list,
    /// each fixture's own dependencies append at the end as discovered,
    /// then a stable sort puts higher-scoped fixtures first. Parametrized
    /// fixtures expand as ID/axis order from this.
    pub fn closure_for(
        &self,
        nodeid: &str,
        requested: &[String],
        ignore_args: &HashSet<String>,
    ) -> Vec<Arc<FixtureDef>> {
        let initialnames = self.initial_names(nodeid, requested);
        let names = self.getfixtureclosure(nodeid, &initialnames, ignore_args);
        // Keep only real, visible fixtures (drops `request` and unknown names);
        // each resolves to its most-specific definition for setup.
        names
            .iter()
            .filter_map(|n| self.lookup(n, nodeid))
            .collect()
    }

    /// Seed names for a closure: autouse fixtures (most general first) followed
    /// by the directly requested names (deduplicated, order preserved).
    pub fn initial_names(&self, nodeid: &str, requested: &[String]) -> Vec<String> {
        let mut seen: HashSet<String> = HashSet::new();
        let mut names: Vec<String> = Vec::new();
        for def in self.autouse_for(nodeid) {
            if seen.insert(def.name.clone()) {
                names.push(def.name.clone());
            }
        }
        for name in requested {
            if seen.insert(name.clone()) {
                names.push(name.clone());
            }
        }
        names
    }

    /// Visible definitions of `name` from `nodeid`, ordered least→most specific
    /// (the last is the most-specific override), like pytest's getfixturedefs.
    pub fn getfixturedefs(&self, name: &str, nodeid: &str) -> Vec<Arc<FixtureDef>> {
        self.by_name
            .get(name)
            .map(|defs| {
                defs.iter()
                    .filter(|def| nodeid.starts_with(&def.baseid))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    /// pytest's FixtureManager.getfixtureclosure: the transitive fixture-name
    /// closure for `initialnames` (already including autouse), expanding through
    /// override chains — a fixture reusing its own name (`app(app)`) reaches the
    /// overridden super's dependencies — then stable-sorted by scope (highest
    /// first). Names in `ignore_args` are kept but their deps are not expanded
    /// (a direct parametrize that shadows a fixture). Includes `request` and
    /// other non-fixture names (appended, never expanded).
    pub fn getfixtureclosure(
        &self,
        nodeid: &str,
        initialnames: &[String],
        ignore_args: &HashSet<String>,
    ) -> Vec<String> {
        let mut closure: Vec<String> = Vec::new();
        {
            let mut seen: HashSet<String> = HashSet::new();
            for n in initialnames {
                if seen.insert(n.clone()) {
                    closure.push(n.clone());
                }
            }
        }
        let mut arg2defs: HashMap<String, Vec<Arc<FixtureDef>>> = HashMap::new();
        let mut current_indices: HashMap<String, i32> = HashMap::new();
        for name in initialnames {
            self.process_argname(
                nodeid,
                name,
                ignore_args,
                &mut closure,
                &mut arg2defs,
                &mut current_indices,
            );
        }
        // Stable sort by the most-specific def's scope (highest first); names
        // with no fixturedef (request, parametrize-only) sort as Function.
        closure.sort_by_key(|n| {
            std::cmp::Reverse(
                arg2defs
                    .get(n)
                    .and_then(|defs| defs.last())
                    .map(|d| d.scope)
                    .unwrap_or(Scope::Function),
            )
        });
        closure
    }

    /// One step of getfixtureclosure's DFS, tracking the override-stack index
    /// per name (negative: -1 most specific) so a fixture reusing its own name
    /// descends to the next less-specific definition.
    #[allow(clippy::only_used_in_recursion)]
    fn process_argname(
        &self,
        nodeid: &str,
        argname: &str,
        ignore_args: &HashSet<String>,
        closure: &mut Vec<String>,
        arg2defs: &mut HashMap<String, Vec<Arc<FixtureDef>>>,
        current_indices: &mut HashMap<String, i32>,
    ) {
        // Already fully processed at the most-specific level.
        if current_indices.get(argname) == Some(&-1) {
            return;
        }
        if !closure.iter().any(|n| n == argname) {
            closure.push(argname.to_string());
        }
        if ignore_args.contains(argname) {
            return;
        }
        if !arg2defs.contains_key(argname) {
            let defs = self.getfixturedefs(argname, nodeid);
            if defs.is_empty() {
                return;
            }
            arg2defs.insert(argname.to_string(), defs);
        }
        let index = *current_indices.get(argname).unwrap_or(&-1);
        let len = arg2defs[argname].len();
        if (-index) as usize > len {
            // Exhausted the override chain (errors at runtime, not here).
            return;
        }
        let dep_names: Vec<String> = arg2defs[argname][(len as i32 + index) as usize]
            .param_names
            .clone();
        current_indices.insert(argname.to_string(), index - 1);
        for dep in &dep_names {
            self.process_argname(nodeid, dep, ignore_args, closure, arg2defs, current_indices);
        }
        current_indices.insert(argname.to_string(), index);
    }

    /// Every argname `def` transitively requests, as visible from `nodeid`,
    /// including `def`'s own name. Non-fixture leaf names (direct parametrize
    /// args like `item`) are kept — they identify the param a dependent
    /// fixture is keyed by. `request` is skipped.
    pub fn transitive_argnames(&self, nodeid: &str, def: &Arc<FixtureDef>) -> HashSet<String> {
        let mut seen: HashSet<String> = HashSet::new();
        seen.insert(def.name.clone());
        let mut queue: Vec<String> = def.param_names.clone();
        while let Some(name) = queue.pop() {
            if name == "request" || !seen.insert(name.clone()) {
                continue;
            }
            // A fixture requesting its own name overrides a less-specific
            // def; the override's deps are reached through that less-specific
            // def, not by re-walking the same name.
            if let Some(dep) = self.lookup(&name, nodeid) {
                queue.extend(dep.param_names.iter().cloned());
            }
        }
        seen
    }

    /// Autouse fixtures visible from `nodeid`, most general first.
    pub fn autouse_for(&self, nodeid: &str) -> Vec<Arc<FixtureDef>> {
        let mut found: Vec<Arc<FixtureDef>> = self
            .by_name
            .values()
            .filter_map(|defs| {
                defs.iter()
                    .rev()
                    .find(|def| def.autouse && nodeid.starts_with(&def.baseid))
                    .cloned()
            })
            .collect();
        // Higher-scoped autouse first (pytest sets up session/module autouse
        // before function ones — pytest-django's session django_test_environment
        // must run before the function _dj_autoclear_mailbox), then
        // most-general baseid, then name for stability.
        found.sort_by(|a, b| {
            std::cmp::Reverse(a.scope)
                .cmp(&std::cmp::Reverse(b.scope))
                .then(a.baseid.len().cmp(&b.baseid.len()))
                .then(a.name.cmp(&b.name))
        });
        found
    }
}
