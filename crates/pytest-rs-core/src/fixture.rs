use std::collections::HashMap;
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

    /// The most specific definition of `name` visible from `nodeid`.
    pub fn lookup(&self, name: &str, nodeid: &str) -> Option<Arc<FixtureDef>> {
        self.by_name.get(name).and_then(|defs| {
            defs.iter()
                .rev()
                .find(|def| nodeid.starts_with(&def.baseid))
                .cloned()
        })
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
        found.sort_by(|a, b| {
            a.baseid
                .len()
                .cmp(&b.baseid.len())
                .then(a.name.cmp(&b.name))
        });
        found
    }
}
