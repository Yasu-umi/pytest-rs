use std::path::{Path, PathBuf};

use pyo3::prelude::*;

/// A marker applied to a test item (e.g. `@pytest.mark.asyncio`).
pub struct MarkData {
    pub name: String,
    /// The Python Mark object (has .args / .kwargs).
    pub obj: Py<PyAny>,
}

/// One runnable, collected test function.
pub struct TestItem {
    /// pytest-compatible node id, e.g. "tests/test_code.py::test_add".
    pub nodeid: String,
    pub path: PathBuf,
    pub module_name: String,
    pub func_name: String,
    /// The test callable itself.
    pub func: Py<PyAny>,
    pub is_coroutine: bool,
    /// Fixture names requested in the test signature.
    pub fixture_names: Vec<String>,
    pub marks: Vec<MarkData>,
}

impl TestItem {
    pub fn get_closest_marker(&self, name: &str) -> Option<&MarkData> {
        self.marks.iter().find(|m| m.name == name)
    }

    /// The cache instance key for module-scoped fixtures of this item.
    pub fn module_instance(&self) -> String {
        self.nodeid
            .split_once("::")
            .map(|(m, _)| m.to_string())
            .unwrap_or_else(|| self.nodeid.clone())
    }
}

/// Expand CLI path arguments into the ordered list of test files.
pub fn collect_test_files(rootdir: &Path, paths: &[String]) -> Result<Vec<PathBuf>, String> {
    let args: Vec<String> = if paths.is_empty() {
        vec![".".to_string()]
    } else {
        paths.to_vec()
    };

    let mut files = Vec::new();
    for arg in &args {
        let path = rootdir.join(arg);
        let meta = std::fs::metadata(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        if meta.is_dir() {
            collect_dir(&path, &mut files)?;
        } else {
            files.push(path);
        }
    }
    Ok(files)
}

fn collect_dir(dir: &Path, files: &mut Vec<PathBuf>) -> Result<(), String> {
    const NORECURSE: &[&str] = &[
        ".git",
        ".venv",
        "venv",
        "node_modules",
        "__pycache__",
        ".tox",
        ".eggs",
        "build",
        "dist",
    ];
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .map_err(|e| format!("{}: {e}", dir.display()))?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if !NORECURSE.contains(&name) && !name.starts_with('.') {
                collect_dir(&path, files)?;
            }
        } else if is_test_file(&path) {
            files.push(path);
        }
    }
    Ok(())
}

/// Default python_files patterns: test_*.py / *_test.py.
pub fn is_test_file(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    name.ends_with(".py") && (name.starts_with("test_") || name.ends_with("_test.py"))
}

/// pytest "prepend" import mode: walk up while __init__.py exists; the first
/// directory without one is the sys.path root, and the dotted module name is
/// the relative path from there.
pub fn module_name_for(path: &Path) -> (PathBuf, String) {
    let mut basedir = path.parent().unwrap_or(Path::new(".")).to_path_buf();
    let mut parts = vec![path.file_stem().unwrap().to_string_lossy().to_string()];
    while basedir.join("__init__.py").exists() {
        parts.push(
            basedir
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default(),
        );
        match basedir.parent() {
            Some(parent) => basedir = parent.to_path_buf(),
            None => break,
        }
    }
    parts.reverse();
    (basedir, parts.join("."))
}

/// Node id for a test file: path relative to rootdir with '/' separators.
pub fn file_nodeid(rootdir: &Path, path: &Path) -> String {
    path.strip_prefix(rootdir)
        .unwrap_or(path)
        .components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}
