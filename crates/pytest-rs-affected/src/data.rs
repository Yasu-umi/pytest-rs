//! The data file (docs/AFFECTED_TESTS.md §4.6, §5.1): one versioned binary
//! file, loaded fully into memory, strings interned, dependency sets
//! deduplicated into groups that many tests share, written atomically.
//!
//! Each test stores its own `(unit, checksum at record time)` pairs (through
//! its groups), so an entry stays valid whenever it was recorded: merging a
//! partial run into older data is safe.

use std::collections::{HashMap, HashSet};
use std::path::Path;

const MAGIC: &[u8] = b"PYTEST-RS-AFFECTED\0";
/// Bumped on any change to the layout below.
pub const FORMAT_VERSION: u32 = 4;

/// The fields that decide whether stored data is usable at all: any
/// mismatch means a full run (and, when recording, a fresh file).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Header {
    pub checksum_scheme: u32,
    pub ruff_tag: String,
    pub pytest_rs_version: String,
    /// `sys.version`, implementation and platform.
    pub python: String,
    /// Hash of every installed distribution's (name, version).
    pub environment: u64,
}

impl Header {
    /// Why `self` (stored) cannot be used for a run whose header is
    /// `current`, or None when it can.
    pub fn mismatch(&self, current: &Header) -> Option<&'static str> {
        if self.checksum_scheme != current.checksum_scheme || self.ruff_tag != current.ruff_tag {
            Some("checksum scheme changed")
        } else if self.pytest_rs_version != current.pytest_rs_version {
            Some("pytest-rs version changed")
        } else if self.python != current.python {
            Some("Python changed")
        } else if self.environment != current.environment {
            Some("installed packages changed")
        } else {
            None
        }
    }
}

/// One recorded dependency: a unit of a file and its checksum at record
/// time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Dep {
    pub file: u32,
    pub qualname: u32,
    pub checksum: u64,
}

/// A recorded file's state at its latest recording: when the current
/// content hash still matches, its units are known without parsing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileEntry {
    pub content_hash: u64,
    /// None when the file did not parse.
    pub units: Option<Vec<(u32, u64)>>,
    /// The commit (interned) this state is the clean, tracked content of,
    /// if it was when recorded: while git reports the file unchanged since
    /// that commit, the state is reused without reading the file (§5.2).
    pub commit: Option<u32>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TestEntry {
    /// The rootdir-relative path of the test's file.
    pub path: u32,
    pub failed: bool,
    pub duration: f64,
    pub groups: Vec<u32>,
    /// The test (or a context it depends on) started a subprocess, whose
    /// dependencies are not recorded (§4.5).
    pub incomplete: bool,
    /// When it was recorded (ms since the epoch): merging keeps the newer
    /// of two entries, whatever order the files are merged in.
    pub recorded: u64,
}

/// A test file's state at its latest import: every node id it collected
/// and the groups every one of its tests depends on (global, the file's
/// import, its static imports). When all of them are unchanged the file
/// need not be imported at all (pre-import selection, §5.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileRecord {
    pub groups: Vec<u32>,
    pub nodeids: Vec<u32>,
    /// As `TestEntry::recorded`.
    pub recorded: u64,
}

/// A non-`.py` file's state when the snapshot was taken (§6: in PR 1 any
/// change to one means a full run).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapEntry {
    pub path: String,
    pub size: u64,
    pub mtime_ns: i64,
    pub hash: u64,
    /// As `FileEntry::commit`.
    pub commit: Option<String>,
}

#[derive(Debug, Default, Clone)]
pub struct Interner {
    strings: Vec<String>,
    index: HashMap<String, u32>,
}

impl Interner {
    pub fn intern(&mut self, value: &str) -> u32 {
        if let Some(&id) = self.index.get(value) {
            return id;
        }
        let id = self.strings.len() as u32;
        self.strings.push(value.to_string());
        self.index.insert(value.to_string(), id);
        id
    }

    pub fn get(&self, value: &str) -> Option<u32> {
        self.index.get(value).copied()
    }

    pub fn resolve(&self, id: u32) -> &str {
        &self.strings[id as usize]
    }
}

#[derive(Debug, Clone)]
pub struct Data {
    pub header: Header,
    pub strings: Interner,
    pub files: HashMap<u32, FileEntry>,
    groups: Vec<Vec<Dep>>,
    group_index: HashMap<Vec<Dep>, u32>,
    pub tests: HashMap<u32, TestEntry>,
    pub file_records: HashMap<u32, FileRecord>,
    pub snapshot: Vec<SnapEntry>,
    /// The latest recording started a subprocess outside every test (a
    /// conftest or plugin at import or session start): every test it
    /// recorded is incomplete.
    pub global_incomplete: bool,
}

/// Why a data file could not be loaded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoadError {
    Missing,
    /// Written by a different data format version.
    OtherFormat,
    Unreadable,
}

/// Re-interns strings and renumbers groups while serializing, so only what
/// is still referenced is written.
struct Compactor<'a> {
    data: &'a Data,
    strings: Interner,
    group_map: HashMap<u32, u32>,
    groups: Vec<Vec<Dep>>,
    used_files: HashSet<u32>,
}

impl Compactor<'_> {
    fn string(&mut self, old: u32) -> u32 {
        self.strings.intern(self.data.strings.resolve(old))
    }

    fn group(&mut self, old: u32) -> u32 {
        if let Some(&new) = self.group_map.get(&old) {
            return new;
        }
        let deps: Vec<Dep> = self.data.groups[old as usize]
            .iter()
            .map(|dep| {
                self.used_files.insert(dep.file);
                Dep {
                    file: self.string(dep.file),
                    qualname: self.string(dep.qualname),
                    checksum: dep.checksum,
                }
            })
            .collect();
        self.groups.push(deps);
        let new = (self.groups.len() - 1) as u32;
        self.group_map.insert(old, new);
        new
    }
}

impl Data {
    pub fn new(header: Header) -> Self {
        Self {
            header,
            strings: Interner::default(),
            files: HashMap::new(),
            groups: Vec::new(),
            group_index: HashMap::new(),
            tests: HashMap::new(),
            file_records: HashMap::new(),
            snapshot: Vec::new(),
            global_incomplete: false,
        }
    }

    /// Store a dependency set (deduplicated against existing groups).
    pub fn add_group(&mut self, mut deps: Vec<Dep>) -> u32 {
        deps.sort_unstable();
        deps.dedup();
        if let Some(&id) = self.group_index.get(&deps) {
            return id;
        }
        let id = self.groups.len() as u32;
        self.group_index.insert(deps.clone(), id);
        self.groups.push(deps);
        id
    }

    pub fn group(&self, id: u32) -> &[Dep] {
        &self.groups[id as usize]
    }

    pub fn groups(&self) -> impl Iterator<Item = &[Dep]> {
        self.groups.iter().map(Vec::as_slice)
    }

    #[cfg(test)]
    pub fn group_count(&self) -> usize {
        self.groups.len()
    }

    pub fn test(&self, nodeid: &str) -> Option<&TestEntry> {
        self.tests.get(&self.strings.get(nodeid)?)
    }

    /// Merge `other` into `self`: its test entries and file records replace
    /// ours for the same node id or path; file states it recorded replace
    /// ours (they are newer). Each test keeps the groups it was recorded
    /// with, so entries from different runs stay valid side by side. Used to
    /// fold `-n` workers' recordings into the controller's data, and by
    /// `--affected-merge`.
    pub fn merge(&mut self, other: &Data) {
        let string = |data: &mut Data, id: u32| data.strings.intern(other.strings.resolve(id));
        let mut group_map: HashMap<u32, u32> = HashMap::new();
        let mut group = |data: &mut Data, old: u32| -> u32 {
            if let Some(&new) = group_map.get(&old) {
                return new;
            }
            let deps: Vec<Dep> = other.groups[old as usize]
                .iter()
                .map(|dep| Dep {
                    file: data.strings.intern(other.strings.resolve(dep.file)),
                    qualname: data.strings.intern(other.strings.resolve(dep.qualname)),
                    checksum: dep.checksum,
                })
                .collect();
            let new = data.add_group(deps);
            group_map.insert(old, new);
            new
        };
        for (path, entry) in &other.files {
            let path = string(self, *path);
            let units = entry.units.as_ref().map(|units| {
                units
                    .iter()
                    .map(|(q, c)| (self.strings.intern(other.strings.resolve(*q)), *c))
                    .collect()
            });
            let commit = entry.commit.map(|c| string(self, c));
            self.files.insert(
                path,
                FileEntry {
                    content_hash: entry.content_hash,
                    units,
                    commit,
                },
            );
        }
        for (nodeid, entry) in &other.tests {
            let nodeid = string(self, *nodeid);
            if self
                .tests
                .get(&nodeid)
                .is_some_and(|ours| ours.recorded > entry.recorded)
            {
                continue;
            }
            let path = string(self, entry.path);
            let groups = entry.groups.iter().map(|g| group(self, *g)).collect();
            self.tests.insert(
                nodeid,
                TestEntry {
                    path,
                    failed: entry.failed,
                    duration: entry.duration,
                    groups,
                    incomplete: entry.incomplete,
                    recorded: entry.recorded,
                },
            );
        }
        for (path, record) in &other.file_records {
            let path = string(self, *path);
            if self
                .file_records
                .get(&path)
                .is_some_and(|ours| ours.recorded > record.recorded)
            {
                continue;
            }
            let groups = record.groups.iter().map(|g| group(self, *g)).collect();
            let nodeids = record.nodeids.iter().map(|n| string(self, *n)).collect();
            self.file_records.insert(
                path,
                FileRecord {
                    groups,
                    nodeids,
                    recorded: record.recorded,
                },
            );
        }
        if !other.snapshot.is_empty() {
            self.snapshot = other.snapshot.clone();
        }
        self.global_incomplete |= other.global_incomplete;
    }

    /// Load a data file. None when it is missing, unreadable or malformed
    /// (the caller then runs everything).
    pub fn load(path: &Path) -> Result<Self, LoadError> {
        let bytes = std::fs::read(path).map_err(|_| LoadError::Missing)?;
        let format = bytes
            .get(MAGIC.len()..MAGIC.len() + 4)
            .filter(|_| bytes.starts_with(MAGIC))
            .map(|v| u32::from_le_bytes(v.try_into().expect("4 bytes")));
        match format {
            Some(version) if version != FORMAT_VERSION => Err(LoadError::OtherFormat),
            _ => Self::decode(&bytes).ok_or(LoadError::Unreadable),
        }
    }

    /// Write atomically: a temp file next to the target, then a rename.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let bytes = self.encode();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = path.with_file_name(format!(
            "{}.tmp-{}",
            path.file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default(),
            std::process::id()
        ));
        std::fs::write(&tmp, bytes)?;
        std::fs::rename(&tmp, path).inspect_err(|_| {
            let _ = std::fs::remove_file(&tmp);
        })
    }

    /// Serialize, keeping only what the tests still reference (unreferenced
    /// groups, files and strings are dropped).
    pub fn encode(&self) -> Vec<u8> {
        let mut nodeids: Vec<(&str, &TestEntry)> = self
            .tests
            .iter()
            .map(|(id, entry)| (self.strings.resolve(*id), entry))
            .collect();
        nodeids.sort_by(|a, b| a.0.cmp(b.0));

        let mut c = Compactor {
            data: self,
            strings: Interner::default(),
            group_map: HashMap::new(),
            groups: Vec::new(),
            used_files: HashSet::new(),
        };
        let tests: Vec<(u32, TestEntry)> = nodeids
            .iter()
            .map(|(nodeid, entry)| {
                let groups = entry.groups.iter().map(|g| c.group(*g)).collect();
                let id = c.strings.intern(nodeid);
                let entry = TestEntry {
                    path: c.string(entry.path),
                    failed: entry.failed,
                    duration: entry.duration,
                    groups,
                    incomplete: entry.incomplete,
                    recorded: entry.recorded,
                };
                (id, entry)
            })
            .collect();
        let mut record_paths: Vec<(&str, &FileRecord)> = self
            .file_records
            .iter()
            .map(|(id, record)| (self.strings.resolve(*id), record))
            .collect();
        record_paths.sort_by(|a, b| a.0.cmp(b.0));
        let records: Vec<(u32, FileRecord)> = record_paths
            .iter()
            .map(|(path, record)| {
                let groups = record.groups.iter().map(|g| c.group(*g)).collect();
                let nodeids = record.nodeids.iter().map(|n| c.string(*n)).collect();
                (
                    c.strings.intern(path),
                    FileRecord {
                        groups,
                        nodeids,
                        recorded: record.recorded,
                    },
                )
            })
            .collect();
        let used: Vec<u32> = c.used_files.iter().copied().collect();
        let mut files: Vec<(u32, FileEntry)> = used
            .iter()
            .filter_map(|old| {
                let entry = self.files.get(old)?;
                let units = entry.units.as_ref().map(|units| {
                    units
                        .iter()
                        .map(|(qualname, checksum)| (c.string(*qualname), *checksum))
                        .collect()
                });
                let commit = entry.commit.map(|commit| c.string(commit));
                Some((
                    c.string(*old),
                    FileEntry {
                        content_hash: entry.content_hash,
                        units,
                        commit,
                    },
                ))
            })
            .collect();
        files.sort_by_key(|(id, _)| *id);
        let Compactor {
            strings, groups, ..
        } = c;

        let mut out = Writer::default();
        out.bytes(MAGIC);
        out.u32(FORMAT_VERSION);
        out.u32(self.header.checksum_scheme);
        out.str(&self.header.ruff_tag);
        out.str(&self.header.pytest_rs_version);
        out.str(&self.header.python);
        out.u64(self.header.environment);
        out.u32(strings.strings.len() as u32);
        strings.strings.iter().for_each(|s| out.str(s));
        out.u32(files.len() as u32);
        files.iter().for_each(|(path, entry)| {
            out.u32(*path);
            out.u64(entry.content_hash);
            out.u32(entry.commit.unwrap_or(u32::MAX));
            match &entry.units {
                None => out.u8(0),
                Some(units) => {
                    out.u8(1);
                    out.u32(units.len() as u32);
                    units.iter().for_each(|(qualname, checksum)| {
                        out.u32(*qualname);
                        out.u64(*checksum);
                    });
                }
            }
        });
        out.u32(groups.len() as u32);
        groups.iter().for_each(|deps| {
            out.u32(deps.len() as u32);
            deps.iter().for_each(|dep| {
                out.u32(dep.file);
                out.u32(dep.qualname);
                out.u64(dep.checksum);
            });
        });
        out.u32(tests.len() as u32);
        tests.iter().for_each(|(nodeid, entry)| {
            out.u32(*nodeid);
            out.u32(entry.path);
            out.u8(u8::from(entry.failed));
            out.f64(entry.duration);
            out.u8(u8::from(entry.incomplete));
            out.u64(entry.recorded);
            out.u32(entry.groups.len() as u32);
            entry.groups.iter().for_each(|group| out.u32(*group));
        });
        out.u32(records.len() as u32);
        records.iter().for_each(|(path, record)| {
            out.u32(*path);
            out.u32(record.groups.len() as u32);
            record.groups.iter().for_each(|group| out.u32(*group));
            out.u32(record.nodeids.len() as u32);
            record.nodeids.iter().for_each(|nodeid| out.u32(*nodeid));
            out.u64(record.recorded);
        });
        out.u32(self.snapshot.len() as u32);
        self.snapshot.iter().for_each(|entry| {
            out.str(&entry.path);
            out.u64(entry.size);
            out.u64(entry.mtime_ns as u64);
            out.u64(entry.hash);
            out.str(entry.commit.as_deref().unwrap_or(""));
        });
        out.u8(u8::from(self.global_incomplete));
        out.buf
    }

    pub fn decode(bytes: &[u8]) -> Option<Self> {
        let mut input = Reader { bytes, pos: 0 };
        if input.take(MAGIC.len())? != MAGIC || input.u32()? != FORMAT_VERSION {
            return None;
        }
        let header = Header {
            checksum_scheme: input.u32()?,
            ruff_tag: input.str()?,
            pytest_rs_version: input.str()?,
            python: input.str()?,
            environment: input.u64()?,
        };
        let mut data = Self::new(header);
        let n_strings = input.u32()?;
        for _ in 0..n_strings {
            let value = input.str()?;
            data.strings.intern(&value);
        }
        let n_strings = data.strings.strings.len() as u32;
        let string_id = |id: u32| (id < n_strings).then_some(id);
        for _ in 0..input.u32()? {
            let path = string_id(input.u32()?)?;
            let content_hash = input.u64()?;
            let commit = match input.u32()? {
                u32::MAX => None,
                id => Some(string_id(id)?),
            };
            let units = match input.u8()? {
                0 => None,
                _ => {
                    let n = input.u32()?;
                    let mut units = Vec::with_capacity(n as usize);
                    for _ in 0..n {
                        units.push((string_id(input.u32()?)?, input.u64()?));
                    }
                    Some(units)
                }
            };
            data.files.insert(
                path,
                FileEntry {
                    content_hash,
                    units,
                    commit,
                },
            );
        }
        for _ in 0..input.u32()? {
            let n = input.u32()?;
            let mut deps = Vec::with_capacity(n as usize);
            for _ in 0..n {
                deps.push(Dep {
                    file: string_id(input.u32()?)?,
                    qualname: string_id(input.u32()?)?,
                    checksum: input.u64()?,
                });
            }
            let id = data.groups.len() as u32;
            data.group_index.insert(deps.clone(), id);
            data.groups.push(deps);
        }
        let n_groups = data.groups.len() as u32;
        for _ in 0..input.u32()? {
            let nodeid = string_id(input.u32()?)?;
            let path = string_id(input.u32()?)?;
            let failed = input.u8()? != 0;
            let duration = input.f64()?;
            let incomplete = input.u8()? != 0;
            let recorded = input.u64()?;
            let groups = input.group_ids(n_groups)?;
            data.tests.insert(
                nodeid,
                TestEntry {
                    path,
                    failed,
                    duration,
                    groups,
                    incomplete,
                    recorded,
                },
            );
        }
        for _ in 0..input.u32()? {
            let path = string_id(input.u32()?)?;
            let groups = input.group_ids(n_groups)?;
            let n = input.u32()?;
            let mut nodeids = Vec::with_capacity(n as usize);
            for _ in 0..n {
                nodeids.push(string_id(input.u32()?)?);
            }
            let recorded = input.u64()?;
            data.file_records.insert(
                path,
                FileRecord {
                    groups,
                    nodeids,
                    recorded,
                },
            );
        }
        for _ in 0..input.u32()? {
            data.snapshot.push(SnapEntry {
                path: input.str()?,
                size: input.u64()?,
                mtime_ns: input.u64()? as i64,
                hash: input.u64()?,
                commit: Some(input.str()?).filter(|c| !c.is_empty()),
            });
        }
        data.global_incomplete = input.u8()? != 0;
        (input.pos == bytes.len()).then_some(data)
    }
}

#[derive(Default)]
struct Writer {
    buf: Vec<u8>,
}

impl Writer {
    fn bytes(&mut self, value: &[u8]) {
        self.buf.extend_from_slice(value);
    }
    fn u8(&mut self, value: u8) {
        self.buf.push(value);
    }
    fn u32(&mut self, value: u32) {
        self.buf.extend_from_slice(&value.to_le_bytes());
    }
    fn u64(&mut self, value: u64) {
        self.buf.extend_from_slice(&value.to_le_bytes());
    }
    fn f64(&mut self, value: f64) {
        self.buf.extend_from_slice(&value.to_le_bytes());
    }
    fn str(&mut self, value: &str) {
        self.u32(value.len() as u32);
        self.bytes(value.as_bytes());
    }
}

struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let end = self.pos.checked_add(n)?;
        let slice = self.bytes.get(self.pos..end)?;
        self.pos = end;
        Some(slice)
    }
    fn u8(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }
    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }
    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }
    fn f64(&mut self) -> Option<f64> {
        Some(f64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }
    fn str(&mut self) -> Option<String> {
        let n = self.u32()? as usize;
        String::from_utf8(self.take(n)?.to_vec()).ok()
    }
    /// A length-prefixed list of group ids, each below `n_groups`.
    fn group_ids(&mut self, n_groups: u32) -> Option<Vec<u32>> {
        let n = self.u32()?;
        let mut groups = Vec::with_capacity(n as usize);
        for _ in 0..n {
            let group = self.u32()?;
            if group >= n_groups {
                return None;
            }
            groups.push(group);
        }
        Some(groups)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header() -> Header {
        Header {
            checksum_scheme: 1,
            ruff_tag: "0.17.5".into(),
            pytest_rs_version: "0.0.15".into(),
            python: "3.13".into(),
            environment: 42,
        }
    }

    fn sample() -> Data {
        let mut data = Data::new(header());
        let file = data.strings.intern("pkg/mod.py");
        let f = data.strings.intern("f");
        let module = data.strings.intern("<module>");
        data.files.insert(
            file,
            FileEntry {
                content_hash: 7,
                units: Some(vec![(f, 1), (module, 2)]),
                commit: Some(data.strings.intern("abc123")),
            },
        );
        let shared = data.add_group(vec![Dep {
            file,
            qualname: module,
            checksum: 2,
        }]);
        let own = data.add_group(vec![Dep {
            file,
            qualname: f,
            checksum: 1,
        }]);
        let test_file = data.strings.intern("tests/test_mod.py");
        for (nodeid, failed) in [
            ("tests/test_mod.py::test_a", false),
            ("tests/test_mod.py::test_b", true),
        ] {
            let id = data.strings.intern(nodeid);
            data.tests.insert(
                id,
                TestEntry {
                    path: test_file,
                    failed,
                    duration: 0.5,
                    groups: vec![shared, own],
                    incomplete: nodeid.ends_with("test_b"),
                    recorded: 200,
                },
            );
        }
        let a = data.strings.intern("tests/test_mod.py::test_a");
        data.file_records.insert(
            test_file,
            FileRecord {
                groups: vec![shared],
                nodeids: vec![a],
                recorded: 200,
            },
        );
        data.global_incomplete = true;
        data.snapshot.push(SnapEntry {
            path: "pyproject.toml".into(),
            size: 10,
            mtime_ns: -1,
            hash: 9,
            commit: Some("abc123".into()),
        });
        data
    }

    #[test]
    fn roundtrip() {
        let data = sample();
        let decoded = Data::decode(&data.encode()).expect("decodes");
        assert_eq!(decoded.header, data.header);
        assert_eq!(decoded.tests.len(), 2);
        let b = decoded.test("tests/test_mod.py::test_b").expect("test_b");
        assert!(b.failed);
        assert!(b.incomplete);
        let record = &decoded.file_records[&b.path];
        assert_eq!(
            decoded.strings.resolve(record.nodeids[0]),
            "tests/test_mod.py::test_a"
        );
        assert_eq!(decoded.group(record.groups[0]).len(), 1);
        assert_eq!(decoded.strings.resolve(b.path), "tests/test_mod.py");
        let deps: Vec<(String, String, u64)> = b
            .groups
            .iter()
            .flat_map(|g| decoded.group(*g).iter())
            .map(|dep| {
                (
                    decoded.strings.resolve(dep.file).to_string(),
                    decoded.strings.resolve(dep.qualname).to_string(),
                    dep.checksum,
                )
            })
            .collect();
        assert_eq!(
            deps,
            [
                ("pkg/mod.py".to_string(), "<module>".to_string(), 2),
                ("pkg/mod.py".to_string(), "f".to_string(), 1),
            ]
        );
        assert_eq!(decoded.snapshot, data.snapshot);
        assert!(decoded.global_incomplete);
        // Re-encoding is deterministic.
        assert_eq!(decoded.encode(), data.encode());
    }

    #[test]
    fn merge_replaces_entries_and_keeps_the_rest() {
        let mut base = Data::new(header());
        let file = base.strings.intern("pkg/mod.py");
        let q = base.strings.intern("f");
        let old_group = base.add_group(vec![Dep {
            file,
            qualname: q,
            checksum: 1,
        }]);
        let test_file = base.strings.intern("tests/test_mod.py");
        for nodeid in ["tests/test_mod.py::test_a", "tests/test_mod.py::test_c"] {
            let id = base.strings.intern(nodeid);
            base.tests.insert(
                id,
                TestEntry {
                    path: test_file,
                    failed: false,
                    duration: 1.0,
                    groups: vec![old_group],
                    incomplete: false,
                    recorded: if nodeid.ends_with("test_c") { 100 } else { 150 },
                },
            );
        }
        let other = sample();
        base.merge(&other);
        // test_a replaced (other's groups), test_b added, test_c kept.
        assert_eq!(base.tests.len(), 3);
        let a = base.test("tests/test_mod.py::test_a").expect("test_a");
        assert_eq!(a.groups.len(), 2);
        let c = base.test("tests/test_mod.py::test_c").expect("test_c");
        assert_eq!(base.group(c.groups[0])[0].checksum, 1);
        assert!(
            base.test("tests/test_mod.py::test_b")
                .expect("test_b")
                .incomplete
        );
        assert!(base.global_incomplete);
        assert_eq!(base.snapshot, other.snapshot);
        // The merged data round-trips.
        let decoded = Data::decode(&base.encode()).expect("decodes");
        assert_eq!(decoded.tests.len(), 3);
        assert_eq!(decoded.file_records.len(), 1);
    }

    #[test]
    fn merge_keeps_the_newer_entry_in_any_order() {
        let newer = sample();
        let mut older = sample();
        for entry in older.tests.values_mut() {
            entry.recorded = 1;
            entry.failed = !entry.failed;
        }
        let mut a = older.clone();
        a.merge(&newer);
        let mut b = newer.clone();
        b.merge(&older);
        for data in [&a, &b] {
            let entry = data.test("tests/test_mod.py::test_b").expect("test_b");
            assert!(entry.failed, "the newer entry wins");
            assert_eq!(entry.recorded, 200);
        }
    }

    #[test]
    fn groups_are_deduplicated() {
        let mut data = sample();
        let n = data.group_count();
        let module = data.strings.get("<module>").expect("interned");
        let file = data.strings.get("pkg/mod.py").expect("interned");
        let again = data.add_group(vec![Dep {
            file,
            qualname: module,
            checksum: 2,
        }]);
        assert_eq!(data.group_count(), n);
        assert_eq!(again, 0);
    }

    #[test]
    fn unreferenced_entries_are_dropped_on_write() {
        let mut data = sample();
        let stale = data.strings.intern("gone.py");
        data.files.insert(
            stale,
            FileEntry {
                content_hash: 1,
                units: None,
                commit: None,
            },
        );
        data.add_group(vec![Dep {
            file: stale,
            qualname: stale,
            checksum: 0,
        }]);
        let decoded = Data::decode(&data.encode()).expect("decodes");
        assert_eq!(decoded.group_count(), 2);
        assert!(decoded.strings.get("gone.py").is_none());
    }

    #[test]
    fn malformed_input_is_rejected() {
        let bytes = sample().encode();
        assert!(Data::decode(&bytes[..bytes.len() - 1]).is_none());
        assert!(Data::decode(b"not a data file").is_none());
        let mut extra = bytes.clone();
        extra.push(0);
        assert!(Data::decode(&extra).is_none());
    }

    #[test]
    fn header_mismatch_reasons() {
        let stored = header();
        assert_eq!(stored.mismatch(&header()), None);
        let mut other = header();
        other.environment = 1;
        assert_eq!(stored.mismatch(&other), Some("installed packages changed"));
        let mut other = header();
        other.python = "3.14".into();
        assert_eq!(stored.mismatch(&other), Some("Python changed"));
    }

    #[test]
    fn save_is_atomic_and_loadable() {
        let dir = std::env::temp_dir().join(format!("affected-data-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let path = dir.join(".pytest-rs-affected");
        sample().save(&path).expect("save");
        let loaded = Data::load(&path).expect("load");
        let mut other = sample().encode();
        other[MAGIC.len()] = 1;
        std::fs::write(&path, other).expect("write");
        assert_eq!(Data::load(&path).err(), Some(LoadError::OtherFormat));
        std::fs::write(&path, b"junk").expect("write");
        assert_eq!(Data::load(&path).err(), Some(LoadError::Unreadable));
        assert_eq!(
            Data::load(&dir.join("absent")).err(),
            Some(LoadError::Missing)
        );
        assert_eq!(loaded.tests.len(), 2);
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .expect("readdir")
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp-"))
            .collect();
        assert!(leftovers.is_empty());
        std::fs::remove_dir_all(&dir).expect("cleanup");
    }
}
