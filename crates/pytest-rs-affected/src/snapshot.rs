//! The non-`.py` snapshot: the content hash of every file under the rootdir
//! that is not Python source (plus Python files listed in a full-run list).
//! PR 1 has no audit hook, so no such file is a recorded dependency: any
//! change among them means a full run (docs/AFFECTED_TESTS.md §10, PR 1).

use std::collections::HashMap;
use std::path::Path;

use crate::config::Globs;
use crate::data::SnapEntry;
use crate::units::FileUnits;

pub struct Snapshot<'a> {
    pub rootdir: &'a Path,
    /// Directory name patterns pytest does not recurse into
    /// (`norecursedirs`).
    pub norecurse: Vec<glob::Pattern>,
    /// Never part of the snapshot.
    pub ignore: Vec<&'a Globs>,
    /// Python files that are part of the snapshot anyway.
    pub include_py: Vec<&'a Globs>,
    /// Rootdir-relative paths of files pytest-rs writes itself (the data
    /// file).
    pub own_files: Vec<String>,
}

/// The difference between two snapshots.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Changes {
    pub paths: Vec<String>,
}

impl Snapshot<'_> {
    /// pytest's default `norecursedirs`.
    pub const DEFAULT_NORECURSE: &[&str] = &[
        "*.egg",
        ".*",
        "_darcs",
        "build",
        "CVS",
        "dist",
        "node_modules",
        "venv",
        "{arch}",
    ];

    /// Walk the rootdir. Files whose size and mtime match `previous` keep
    /// their stored hash; everything else is hashed.
    pub fn take(&self, previous: &[SnapEntry]) -> Vec<SnapEntry> {
        let known: HashMap<&str, &SnapEntry> =
            previous.iter().map(|e| (e.path.as_str(), e)).collect();
        let mut out = Vec::new();
        self.walk(self.rootdir, "", &known, &mut out);
        out.sort_by(|a, b| a.path.cmp(&b.path));
        out
    }

    fn walk(
        &self,
        dir: &Path,
        rel: &str,
        known: &HashMap<&str, &SnapEntry>,
        out: &mut Vec<SnapEntry>,
    ) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        let mut entries: Vec<_> = entries.filter_map(Result::ok).collect();
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            let name = entry.file_name().to_string_lossy().into_owned();
            let child_rel = if rel.is_empty() {
                name.clone()
            } else {
                format!("{rel}/{name}")
            };
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            let path = entry.path();
            if file_type.is_dir() {
                let skip = name == "__pycache__"
                    || self.norecurse.iter().any(|p| p.matches(&name))
                    || path.join("pyvenv.cfg").exists()
                    || self.ignore.iter().any(|g| g.covers_dir(&child_rel));
                if !skip {
                    self.walk(&path, &child_rel, known, out);
                }
                continue;
            }
            if !file_type.is_file() {
                continue;
            }
            let is_py = name.ends_with(".py");
            if (is_py && !self.include_py.iter().any(|g| g.matches(&child_rel)))
                || self.own_files.contains(&child_rel)
                || self.ignore.iter().any(|g| g.matches(&child_rel))
            {
                continue;
            }
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            let size = meta.len();
            let mtime_ns = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_nanos() as i64)
                .unwrap_or(-1);
            let hash = match known.get(child_rel.as_str()) {
                Some(old) if old.size == size && old.mtime_ns == mtime_ns && mtime_ns >= 0 => {
                    old.hash
                }
                _ => match std::fs::read(&path) {
                    Ok(bytes) => FileUnits::content_hash(&bytes),
                    Err(_) => continue,
                },
            };
            out.push(SnapEntry {
                path: child_rel,
                size,
                mtime_ns,
                hash,
            });
        }
    }

    /// Paths added, removed or modified between two snapshots, sorted.
    pub fn diff(before: &[SnapEntry], after: &[SnapEntry]) -> Changes {
        let old: HashMap<&str, u64> = before.iter().map(|e| (e.path.as_str(), e.hash)).collect();
        let new: HashMap<&str, u64> = after.iter().map(|e| (e.path.as_str(), e.hash)).collect();
        let mut paths: Vec<String> = old
            .iter()
            .filter(|(path, hash)| new.get(*path) != Some(hash))
            .map(|(path, _)| path.to_string())
            .chain(
                new.keys()
                    .filter(|path| !old.contains_key(*path))
                    .map(|path| path.to_string()),
            )
            .collect();
        paths.sort();
        Changes { paths }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("affected-snap-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        dir
    }

    #[test]
    fn walk_and_diff() {
        let root = tmpdir("walk");
        std::fs::write(root.join("pyproject.toml"), "[tool]\n").expect("write");
        std::fs::write(root.join("setup.py"), "x = 1\n").expect("write");
        std::fs::write(root.join("mod.py"), "x = 1\n").expect("write");
        std::fs::write(root.join(".coverage"), "db").expect("write");
        std::fs::create_dir_all(root.join("data")).expect("mkdir");
        std::fs::write(root.join("data/a.json"), "{}").expect("write");
        std::fs::create_dir_all(root.join(".git")).expect("mkdir");
        std::fs::write(root.join(".git/HEAD"), "ref").expect("write");
        std::fs::create_dir_all(root.join("docs")).expect("mkdir");
        std::fs::write(root.join("docs/x.md"), "x").expect("write");
        std::fs::create_dir_all(root.join("env")).expect("mkdir");
        std::fs::write(root.join("env/pyvenv.cfg"), "").expect("write");
        std::fs::write(root.join("env/thing.txt"), "").expect("write");

        let builtin_ignore = Globs::builtin(crate::config::BUILTIN_IGNORE);
        let user_ignore = Globs::new(&["docs/**".to_string()]).expect("valid");
        let full_run = Globs::builtin(crate::config::BUILTIN_FULL_RUN_ON);
        let snapshot = Snapshot {
            rootdir: &root,
            norecurse: Snapshot::DEFAULT_NORECURSE
                .iter()
                .map(|p| glob::Pattern::new(p).expect("valid"))
                .collect(),
            ignore: vec![&builtin_ignore, &user_ignore],
            include_py: vec![&full_run],
            own_files: vec![".pytest-rs-affected".to_string()],
        };
        let first = snapshot.take(&[]);
        let paths: Vec<&str> = first.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(paths, ["data/a.json", "pyproject.toml", "setup.py"]);

        assert_eq!(
            Snapshot::diff(&first, &snapshot.take(&first)),
            Changes::default()
        );

        std::fs::write(root.join("data/a.json"), "{\"a\": 1}").expect("write");
        std::fs::write(root.join("data/b.json"), "{}").expect("write");
        std::fs::remove_file(root.join("setup.py")).expect("rm");
        let second = snapshot.take(&first);
        assert_eq!(
            Snapshot::diff(&first, &second).paths,
            ["data/a.json", "data/b.json", "setup.py"]
        );
        std::fs::remove_dir_all(&root).expect("cleanup");
    }
}
