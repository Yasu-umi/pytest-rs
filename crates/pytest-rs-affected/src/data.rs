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
pub const FORMAT_VERSION: u32 = 1;

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
}

#[derive(Debug, Clone, PartialEq)]
pub struct TestEntry {
    /// The rootdir-relative path of the test's file.
    pub path: u32,
    pub failed: bool,
    pub duration: f64,
    pub groups: Vec<u32>,
}

/// A non-`.py` file's state when the snapshot was taken (§6: in PR 1 any
/// change to one means a full run).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapEntry {
    pub path: String,
    pub size: u64,
    pub mtime_ns: i64,
    pub hash: u64,
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
    pub snapshot: Vec<SnapEntry>,
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
            snapshot: Vec::new(),
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

    #[cfg(test)]
    pub fn group_count(&self) -> usize {
        self.groups.len()
    }

    pub fn test(&self, nodeid: &str) -> Option<&TestEntry> {
        self.tests.get(&self.strings.get(nodeid)?)
    }

    /// Load a data file. None when it is missing, unreadable or malformed
    /// (the caller then runs everything).
    pub fn load(path: &Path) -> Option<Self> {
        let bytes = std::fs::read(path).ok()?;
        Self::decode(&bytes)
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

        let mut strings = Interner::default();
        let mut group_map: HashMap<u32, u32> = HashMap::new();
        let mut groups: Vec<Vec<Dep>> = Vec::new();
        let mut used_files: HashSet<u32> = HashSet::new();
        let remap = |old: u32, strings: &mut Interner| strings.intern(self.strings.resolve(old));
        let tests: Vec<(u32, TestEntry)> = nodeids
            .iter()
            .map(|(nodeid, entry)| {
                let groups_new: Vec<u32> = entry
                    .groups
                    .iter()
                    .map(|old| {
                        *group_map.entry(*old).or_insert_with(|| {
                            let deps: Vec<Dep> = self.groups[*old as usize]
                                .iter()
                                .map(|dep| {
                                    used_files.insert(dep.file);
                                    Dep {
                                        file: remap(dep.file, &mut strings),
                                        qualname: remap(dep.qualname, &mut strings),
                                        checksum: dep.checksum,
                                    }
                                })
                                .collect();
                            groups.push(deps);
                            (groups.len() - 1) as u32
                        })
                    })
                    .collect();
                let id = strings.intern(nodeid);
                let entry = TestEntry {
                    path: remap(entry.path, &mut strings),
                    failed: entry.failed,
                    duration: entry.duration,
                    groups: groups_new,
                };
                (id, entry)
            })
            .collect();
        let mut files: Vec<(u32, FileEntry)> = used_files
            .iter()
            .filter_map(|old| {
                let entry = self.files.get(old)?;
                let units = entry.units.as_ref().map(|units| {
                    units
                        .iter()
                        .map(|(qualname, checksum)| (remap(*qualname, &mut strings), *checksum))
                        .collect()
                });
                Some((
                    remap(*old, &mut strings),
                    FileEntry {
                        content_hash: entry.content_hash,
                        units,
                    },
                ))
            })
            .collect();
        files.sort_by_key(|(id, _)| *id);

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
            out.u32(entry.groups.len() as u32);
            entry.groups.iter().for_each(|group| out.u32(*group));
        });
        out.u32(self.snapshot.len() as u32);
        self.snapshot.iter().for_each(|entry| {
            out.str(&entry.path);
            out.u64(entry.size);
            out.u64(entry.mtime_ns as u64);
            out.u64(entry.hash);
        });
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
            let n = input.u32()?;
            let mut groups = Vec::with_capacity(n as usize);
            for _ in 0..n {
                let group = input.u32()?;
                if group >= n_groups {
                    return None;
                }
                groups.push(group);
            }
            data.tests.insert(
                nodeid,
                TestEntry {
                    path,
                    failed,
                    duration,
                    groups,
                },
            );
        }
        for _ in 0..input.u32()? {
            data.snapshot.push(SnapEntry {
                path: input.str()?,
                size: input.u64()?,
                mtime_ns: input.u64()? as i64,
                hash: input.u64()?,
            });
        }
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
                },
            );
        }
        data.snapshot.push(SnapEntry {
            path: "pyproject.toml".into(),
            size: 10,
            mtime_ns: -1,
            hash: 9,
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
        // Re-encoding is deterministic.
        assert_eq!(decoded.encode(), data.encode());
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
