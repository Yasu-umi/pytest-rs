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
    /// pytest-compatible node id, e.g. "tests/test_code.py::test_add[1-2]".
    pub nodeid: String,
    pub path: PathBuf,
    pub module_name: String,
    pub func_name: String,
    /// The test callable itself (unbound function for class methods).
    pub func: Py<PyAny>,
    /// The Test* class this method belongs to, if any. A fresh instance is
    /// created per test at setup.
    pub cls: Option<Py<PyAny>>,
    pub is_coroutine: bool,
    pub is_doctest: bool,
    /// Fixture names requested in the test signature (parametrize-provided
    /// names included; the runner skips resolving those).
    pub fixture_names: Vec<String>,
    /// Pseudo-fixture names visible in request.fixturenames but never
    /// resolved or passed to the test (e.g. _asyncio_loop_factory).
    pub extra_fixture_names: Vec<String>,
    pub marks: Vec<MarkData>,
    /// Direct parameters from @pytest.mark.parametrize, by argname.
    pub callspec: Vec<(String, Py<PyAny>)>,
    /// Parametrized-fixture assignments: (fixture name, param index, value).
    pub fixture_params: Vec<(String, usize, Py<PyAny>)>,
    /// 1-based line of the test definition (0 if unknown).
    pub lineno: u32,
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

    /// The class-scope cache/teardown key: everything before the final
    /// "::" ("file.py::TestClass" for methods, the file for plain tests).
    pub fn class_instance(&self) -> String {
        self.nodeid
            .rsplit_once("::")
            .map(|(prefix, _)| prefix.to_string())
            .unwrap_or_else(|| self.nodeid.clone())
    }
}

/// Expand CLI path arguments into the ordered list of test files.
pub fn collect_test_files(
    invocation_dir: &Path,
    paths: &[String],
    collect_in_virtualenv: bool,
    python_files: &[String],
    keep_duplicates: bool,
) -> Result<Vec<PathBuf>, String> {
    let args: Vec<String> = if paths.is_empty() {
        vec![".".to_string()]
    } else {
        paths.to_vec()
    };

    let mut files = Vec::new();
    for arg in &args {
        // Node-id args select within a file; only the path part is
        // collected here (the engine filters items afterwards).
        let arg = arg.split("::").next().unwrap_or(arg);
        // Canonicalize so symlinked paths (e.g. /tmp on macOS) match the
        // canonical rootdir when computing node ids.
        let path = invocation_dir.join(arg);
        let path = std::fs::canonicalize(&path).unwrap_or(path);
        let meta = std::fs::metadata(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        if meta.is_dir() {
            // An explicitly given directory is collected even if it is a
            // virtualenv root; only recursion skips them.
            collect_dir(&path, files.as_mut(), collect_in_virtualenv, python_files)?;
        } else if keep_duplicates || !files.contains(&path) {
            // --keep-duplicates: a file given twice collects twice (pytest
            // keeps duplicated args).
            files.push(path);
        }
    }
    Ok(files)
}

/// A virtual environment root: PEP-405 pyvenv.cfg, or a conda env
/// (conda-meta/history, which conda creates without pyvenv.cfg).
fn in_venv(path: &Path) -> bool {
    path.join("pyvenv.cfg").is_file() || path.join("conda-meta").join("history").is_file()
}

fn collect_dir(
    dir: &Path,
    files: &mut Vec<PathBuf>,
    collect_in_virtualenv: bool,
    python_files: &[String],
) -> Result<(), String> {
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
            if !NORECURSE.contains(&name)
                && !name.starts_with('.')
                && (collect_in_virtualenv || !in_venv(&path))
            {
                collect_dir(&path, files, collect_in_virtualenv, python_files)?;
            }
        } else if is_test_file(&path, python_files) && path.is_file() && !files.contains(&path) {
            // Overlapping arguments ("pytest a a/b") collect each file
            // once; is_file also drops broken symlinks.
            files.push(path);
        }
    }
    Ok(())
}

/// A file collected during directory recursion: its name matches one of the
/// python_files patterns (default test_*.py / *_test.py). conftest.py never
/// collects as a test module, whatever the patterns say.
pub fn is_test_file(path: &Path, python_files: &[String]) -> bool {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    name != "conftest.py"
        && python_files
            .iter()
            .any(|pattern| wildcard_match(pattern, name))
}

/// fnmatch-style match supporting * and ? (iterative, no allocation).
fn wildcard_match(pattern: &str, name: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let name: Vec<char> = name.chars().collect();
    let (mut p, mut n) = (0usize, 0usize);
    let mut star: Option<(usize, usize)> = None;
    while n < name.len() {
        if p < pattern.len() && (pattern[p] == '?' || pattern[p] == name[n]) {
            p += 1;
            n += 1;
        } else if p < pattern.len() && pattern[p] == '*' {
            star = Some((p, n));
            p += 1;
        } else if let Some((sp, sn)) = star {
            p = sp + 1;
            n = sn + 1;
            star = Some((sp, sn + 1));
        } else {
            return false;
        }
    }
    while p < pattern.len() && pattern[p] == '*' {
        p += 1;
    }
    p == pattern.len()
}

/// pytest "prepend" import mode: walk up while __init__.py exists; the first
/// directory without one is the sys.path root, and the dotted module name is
/// the relative path from there.
pub fn module_name_for(path: &Path) -> (PathBuf, String) {
    let mut basedir = path.parent().unwrap_or(Path::new(".")).to_path_buf();
    let stem = path.file_stem().unwrap().to_string_lossy().to_string();
    // pkg/__init__.py imports as package "pkg", not "pkg.__init__".
    let mut parts = if stem == "__init__" { vec![] } else { vec![stem] };
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

/// Collect all Python files (including non-test files like __init__.py) for
/// --doctest-modules. Does not include files already covered by collect_test_files.
pub fn collect_all_python_files(
    invocation_dir: &Path,
    paths: &[String],
    collect_in_virtualenv: bool,
    already_collected: &[PathBuf],
) -> Vec<PathBuf> {
    let args: Vec<String> = if paths.is_empty() {
        vec![".".to_string()]
    } else {
        paths.to_vec()
    };
    let mut files = Vec::new();
    for arg in &args {
        let arg = arg.split("::").next().unwrap_or(arg);
        let path = invocation_dir.join(arg);
        let path = std::fs::canonicalize(&path).unwrap_or(path);
        if path.is_dir() {
            collect_all_py_dir(&path, &mut files, collect_in_virtualenv);
        } else if path.extension().and_then(|e| e.to_str()) == Some("py")
            && !files.contains(&path)
            && !already_collected.contains(&path)
        {
            files.push(path);
        }
    }
    // Exclude files that were already collected as test files.
    files.retain(|f| !already_collected.contains(f));
    files
}

fn collect_all_py_dir(dir: &Path, files: &mut Vec<PathBuf>, collect_in_virtualenv: bool) {
    const NORECURSE: &[&str] = &[
        ".git", ".venv", "venv", "node_modules", "__pycache__", ".tox", ".eggs", "build", "dist",
    ];
    let Ok(read_dir) = std::fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<_> = read_dir
        .filter_map(Result::ok)
        .map(|e| e.path())
        .collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if !NORECURSE.contains(&name)
                && !name.starts_with('.')
                && (collect_in_virtualenv || !in_venv(&path))
            {
                collect_all_py_dir(&path, files, collect_in_virtualenv);
            }
        } else if path.extension().and_then(|e| e.to_str()) == Some("py")
            && path.is_file()
            && !files.contains(&path)
        {
            files.push(path);
        }
    }
}

/// Gather non-Python files from the search paths for --doctest-glob matching.
/// Walks the same directories as collect_test_files but collects all non-Python files.
pub fn collect_doctest_textfiles(invocation_dir: &Path, paths: &[String]) -> Vec<PathBuf> {
    let args: Vec<String> = if paths.is_empty() {
        vec![".".to_string()]
    } else {
        paths.to_vec()
    };
    let mut files = Vec::new();
    for arg in &args {
        let arg = arg.split("::").next().unwrap_or(arg);
        let path = invocation_dir.join(arg);
        let path = std::fs::canonicalize(&path).unwrap_or(path);
        if path.is_dir() {
            collect_textfiles_dir(&path, &mut files);
        } else if path.is_file() && !is_python_file(&path) && !files.contains(&path) {
            files.push(path);
        }
    }
    files
}

fn collect_textfiles_dir(dir: &Path, files: &mut Vec<PathBuf>) {
    const NORECURSE: &[&str] = &[
        ".git", ".venv", "venv", "node_modules", "__pycache__", ".tox", ".eggs", "build", "dist",
    ];
    let Ok(read_dir) = std::fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<_> = read_dir
        .filter_map(Result::ok)
        .map(|e| e.path())
        .collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if !NORECURSE.contains(&name) && !name.starts_with('.') {
                collect_textfiles_dir(&path, files);
            }
        } else if path.is_file() && !is_python_file(&path) && !files.contains(&path) {
            files.push(path);
        }
    }
}

fn is_python_file(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()) == Some("py")
}

/// Node id for a test file: path relative to rootdir with '/' separators,
/// or the path as-is when it lives outside the rootdir.
pub fn file_nodeid(rootdir: &Path, path: &Path) -> String {
    match path.strip_prefix(rootdir) {
        Ok(relative) => relative
            .components()
            .map(|c| c.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/"),
        Err(_) => path.to_string_lossy().replace('\\', "/"),
    }
}
