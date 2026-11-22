//! Deciding what runs (docs/AFFECTED_TESTS.md §5.2, §5.3, §6): which test
//! files need importing at all, and which items of the imported files run.
//!
//! A test runs when it is forced (explicit node id, `always_run`, a
//! `file_triggers` glob), new, failed last time, recorded with incomplete
//! dependencies, or when any dependency it recorded changed. A test file is
//! not imported when every test it collected last time would be
//! deselected and the code run by its import is unchanged.

use std::collections::HashMap;
use std::path::Path;

use crate::collector::InputKind;
use crate::config::{
    AffectedConfig, BUILTIN_FULL_RUN_ON, Globs, Incomplete, Trigger, UnknownFiles,
};
use crate::data::{Data, Dep, SnapEntry};
use crate::snapshot::Snapshot;
use crate::units::{FILE_UNIT, FileUnits};

/// A stored Python file's state now.
enum CurrentFile {
    /// Missing, unreadable, unparseable or configured as always changed:
    /// every unit counts as changed.
    Changed,
    Units {
        content_hash: u64,
        units: HashMap<String, u64>,
    },
}

/// Why a test runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Reason {
    Affected,
    New,
    Failed,
    Incomplete,
    Forced,
}

impl Reason {
    pub fn label(self) -> &'static str {
        match self {
            Reason::Affected => "affected",
            Reason::New => "new",
            Reason::Failed => "failed",
            Reason::Incomplete => "incomplete deps",
            Reason::Forced => "forced",
        }
    }
}

pub struct Plan {
    /// Why everything runs, if it does.
    pub full_reason: Option<String>,
    /// Test globs from `file_triggers` entries whose files changed.
    pub triggered: Vec<Globs>,
    /// Explicit node ids from the command line (rootdir-relative).
    explicit: Vec<String>,
    current: HashMap<u32, CurrentFile>,
    inputs: HashMap<(u32, InputKind), u64>,
    group_changed: HashMap<u32, bool>,
}

impl Plan {
    /// Classify the non-`.py` changes and load the current state of every
    /// file the stored data refers to (parsing only changed Python files).
    pub fn build(
        data: &Data,
        rootdir: &Path,
        config: &AffectedConfig,
        snapshot: &[SnapEntry],
        explicit: Vec<String>,
        vouched: &dyn Fn(&str, Option<&str>) -> bool,
    ) -> Self {
        let changes = Snapshot::diff(&data.snapshot, snapshot).paths;
        let builtin_full = Globs::builtin(BUILTIN_FULL_RUN_ON);
        let mut full: Vec<&str> = Vec::new();
        let mut triggered: Vec<Globs> = Vec::new();
        for path in &changes {
            let triggers = config.triggers_for(path);
            if builtin_full.matches(path)
                || config.full_run_on.matches(path)
                || triggers.iter().any(|t| matches!(t, Trigger::All))
                || (triggers.is_empty() && config.unknown_files == UnknownFiles::All)
            {
                full.push(path);
            }
            triggered.extend(triggers.into_iter().filter_map(|t| match t {
                Trigger::Tests(globs) => Some(globs.clone()),
                _ => None,
            }));
        }
        let full_reason = (!full.is_empty()).then(|| {
            let shown: Vec<&str> = full.iter().take(3).copied().collect();
            let more = full.len() - shown.len();
            if more > 0 {
                format!("changed: {} and {more} more", shown.join(", "))
            } else {
                format!("changed: {}", shown.join(", "))
            }
        });
        let current = Self::current_units(data, rootdir, config, vouched);
        let inputs = Self::current_inputs(data, rootdir, config);
        Self {
            full_reason,
            triggered,
            explicit,
            current,
            inputs,
            group_changed: HashMap::new(),
        }
    }

    /// `vouched(path, commit)`: git vouches that the file is unchanged since
    /// the commit its stored state was recorded clean at; its stored state
    /// is then current, and the file is not read.
    fn current_units(
        data: &Data,
        rootdir: &Path,
        config: &AffectedConfig,
        vouched: &dyn Fn(&str, Option<&str>) -> bool,
    ) -> HashMap<u32, CurrentFile> {
        data.files
            .iter()
            .map(|(&file, entry)| {
                let rel = data.strings.resolve(file);
                if config.always_changed.matches(rel) {
                    return (file, CurrentFile::Changed);
                }
                let commit = entry.commit.map(|c| data.strings.resolve(c));
                if vouched(rel, commit) {
                    let current = match &entry.units {
                        Some(units) => CurrentFile::Units {
                            content_hash: entry.content_hash,
                            units: units
                                .iter()
                                .map(|(q, c)| (data.strings.resolve(*q).to_string(), *c))
                                .collect(),
                        },
                        None => CurrentFile::Changed,
                    };
                    return (file, current);
                }
                let Ok(bytes) = std::fs::read(rootdir.join(rel)) else {
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

    /// Current hashes of the recorded non-Python inputs (files read,
    /// directories listed). `always_changed` paths get a value no recorded
    /// hash can equal.
    fn current_inputs(
        data: &Data,
        rootdir: &Path,
        config: &AffectedConfig,
    ) -> HashMap<(u32, InputKind), u64> {
        let mut out = HashMap::new();
        for deps in data.groups() {
            for dep in deps {
                let Some(kind) = InputKind::from_qualname(data.strings.resolve(dep.qualname))
                else {
                    continue;
                };
                out.entry((dep.file, kind)).or_insert_with(|| {
                    let rel = data.strings.resolve(dep.file);
                    if config.always_changed.matches(rel) {
                        u64::MAX
                    } else {
                        kind.hash(&rootdir.join(rel))
                    }
                });
            }
        }
        out
    }

    fn dep_changed(&self, data: &Data, dep: &Dep) -> bool {
        let qualname = data.strings.resolve(dep.qualname);
        if let Some(kind) = InputKind::from_qualname(qualname) {
            return self.inputs.get(&(dep.file, kind)) != Some(&dep.checksum);
        }
        match self.current.get(&dep.file) {
            None | Some(CurrentFile::Changed) => true,
            Some(CurrentFile::Units {
                content_hash,
                units,
            }) => {
                if qualname == FILE_UNIT {
                    *content_hash != dep.checksum
                } else {
                    units.get(qualname) != Some(&dep.checksum)
                }
            }
        }
    }

    fn group_changed(&mut self, data: &Data, group: u32) -> bool {
        if let Some(&changed) = self.group_changed.get(&group) {
            return changed;
        }
        let changed = data
            .group(group)
            .iter()
            .any(|dep| self.dep_changed(data, dep));
        self.group_changed.insert(group, changed);
        changed
    }

    fn forced(&self, config: &AffectedConfig, path: &str, nodeid: Option<&str>) -> bool {
        config.always_run.matches(path)
            || self.triggered.iter().any(|g| g.matches(path))
            || match nodeid {
                Some(nodeid) => Self::is_explicit(&self.explicit, nodeid),
                None => self
                    .explicit
                    .iter()
                    .any(|e| e.split("::").next() == Some(path)),
            }
    }

    pub fn is_explicit(explicit: &[String], nodeid: &str) -> bool {
        explicit.iter().any(|prefix| {
            nodeid == prefix
                || nodeid
                    .strip_prefix(prefix.as_str())
                    .is_some_and(|rest| rest.starts_with("::") || rest.starts_with('['))
        })
    }

    /// Why the test runs, or None when it can be deselected.
    pub fn reason(
        &mut self,
        data: &Data,
        config: &AffectedConfig,
        nodeid: &str,
        path: &str,
    ) -> Option<Reason> {
        if self.full_reason.is_some() || self.forced(config, path, Some(nodeid)) {
            return Some(Reason::Forced);
        }
        let Some(entry) = data.test(nodeid) else {
            return Some(Reason::New);
        };
        if entry.failed {
            return Some(Reason::Failed);
        }
        if entry.incomplete && config.incomplete == Incomplete::Run {
            return Some(Reason::Incomplete);
        }
        let groups = entry.groups.clone();
        groups
            .into_iter()
            .any(|g| self.group_changed(data, g))
            .then_some(Reason::Affected)
    }

    /// Whether a test file can stay unimported: it was recorded, the code
    /// its import ran is unchanged, and every test it collected would be
    /// deselected.
    pub fn file_unchanged(&mut self, data: &Data, config: &AffectedConfig, path: &str) -> bool {
        if self.full_reason.is_some() || self.forced(config, path, None) {
            return false;
        }
        let Some(record) = data
            .strings
            .get(path)
            .and_then(|id| data.file_records.get(&id))
        else {
            return false;
        };
        let groups = record.groups.clone();
        if groups.into_iter().any(|g| self.group_changed(data, g)) {
            return false;
        }
        record.nodeids.iter().all(|nodeid| {
            self.reason(data, config, data.strings.resolve(*nodeid), path)
                .is_none()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_prefixes() {
        let explicit = vec!["tests/test_a.py::test_x".to_string()];
        assert!(Plan::is_explicit(&explicit, "tests/test_a.py::test_x"));
        assert!(Plan::is_explicit(&explicit, "tests/test_a.py::test_x[1]"));
        assert!(!Plan::is_explicit(&explicit, "tests/test_a.py::test_xy"));
        let class = vec!["tests/test_a.py::TestK".to_string()];
        assert!(Plan::is_explicit(&class, "tests/test_a.py::TestK::test_m"));
    }
}
