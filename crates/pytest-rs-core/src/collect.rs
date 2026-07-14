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
    /// Fixture names a `pytest_collection_modifyitems` hook injected into the
    /// node's `fixturenames` list (e.g. pytest-order's `error-on-failed-ordering`
    /// pushing a nonexistent name to force a setup error). Unlike
    /// `extra_fixture_names`, these ARE attempted for resolution at setup and
    /// raise "fixture 'X' not found" if unregistered, matching upstream where
    /// `item.fixturenames` itself drives fixture setup.
    pub injected_fixture_names: Vec<String>,
    pub marks: Vec<MarkData>,
    /// Direct parameters from @pytest.mark.parametrize, by argname.
    pub callspec: Vec<(String, Py<PyAny>)>,
    /// Parametrized-fixture assignments: (fixture name, param index, value).
    pub fixture_params: Vec<(String, usize, Py<PyAny>)>,
    /// 1-based line of the test definition (0 if unknown).
    pub lineno: u32,
    /// Python collector class name for custom file collectors (e.g. "MyModule").
    /// Empty string means standard Module collection.
    pub collector_class: String,
    /// Python class name for a custom function/item node produced by
    /// `pytest_pycollect_makeitem` (e.g. "MyFunction"). Empty string means the
    /// standard Function node, rendered as `<Function name>`.
    pub func_class: String,
    /// The Python node object returned by `pytest_pycollect_makeitem`, if any.
    /// When set, `make_py_node` uses it directly instead of constructing a
    /// new `Function` so custom subclasses (with overridden `reportinfo` etc.)
    /// and attributes set by wrapper hooks are preserved.
    pub py_node: Option<Py<PyAny>>,
    /// Highest parametrize scope across all dimensions (for item reordering).
    pub max_param_scope: crate::fixture::Scope,
    /// Per non-function-scoped parametrized arg: (argname, scope, index). The
    /// index is the callspec serial for direct params (upstream
    /// `_recompute_direct_params_indices`) or the value index for indirect
    /// params. `reorder_items` groups items sharing a high-scope param value
    /// (same argname + index + scope boundary), matching pytest.
    pub scope_sort_keys: Vec<(String, crate::fixture::Scope, usize)>,
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

    /// The class-scope cache/teardown key. For a method it is everything
    /// before the final "::" ("file.py::TestClass"). For a plain module-level
    /// test there is no enclosing class, so — mirroring pytest's
    /// `FixtureRequest.node` fallback (`get_scope_node` returns None for class
    /// scope, falling back to the function item) — the key is the full nodeid,
    /// making class-scoped fixtures cache and tear down per-item rather than
    /// being shared across the file.
    pub fn class_instance(&self) -> String {
        match self.nodeid.rsplit_once("::") {
            Some((prefix, _)) if prefix.contains("::") => prefix.to_string(),
            _ => self.nodeid.clone(),
        }
    }

    /// The Package-scope cache/teardown key: the module's enclosing directory
    /// (pytest's Package node identity), not the module file itself — a
    /// package-scoped fixture stays cached across every module in that
    /// directory rather than being re-created per file.
    pub fn package_instance(&self) -> String {
        self.module_instance()
            .rsplit_once('/')
            .map(|(dir, _)| dir.to_string())
            .unwrap_or_default()
    }

    /// The scope-instance string a fixture of `scope` is cached/torn down
    /// under for this item (the instance the scope stays constant within).
    pub fn instance_at(&self, scope: crate::fixture::Scope) -> String {
        use crate::fixture::Scope;
        match scope {
            Scope::Function => self.nodeid.clone(),
            Scope::Class => self.class_instance(),
            Scope::Package => self.package_instance(),
            Scope::Module | Scope::Session => self.module_instance(),
        }
    }

    /// The `repr()` of this item's parametrized value for `argname`, whether
    /// it is a direct param (`callspec`) or an indirect fixture param
    /// (`fixture_params`). Used as the parametrization-binding key so teardown
    /// keys on the value, not the per-function param index (which differs
    /// across functions parametrizing the same fixture, e.g. issue634).
    pub fn param_value_repr(&self, py: Python<'_>, argname: &str) -> Option<String> {
        if let Some((_, _, value)) = self.fixture_params.iter().find(|(n, _, _)| n == argname) {
            return value.bind(py).repr().ok().map(|r| r.to_string());
        }
        if let Some((_, value)) = self.callspec.iter().find(|(n, _)| n == argname) {
            return value.bind(py).repr().ok().map(|r| r.to_string());
        }
        None
    }

    /// Parametrization bindings of `self` (the previous item) in one of `scopes`
    /// whose value-group ends as the run moves on to `next` within the same
    /// scope-instance: the param advances to a new value or is no longer
    /// requested. A node-boundary change (different scope-instance) is excluded
    /// — the deferred scope teardown covers it. Fixtures carrying these
    /// bindings must be torn down before `next` sets the new value up.
    pub fn ended_param_bindings(
        &self,
        py: Python<'_>,
        next: &TestItem,
        scopes: &[crate::fixture::Scope],
    ) -> Vec<crate::session::Binding> {
        self.scope_sort_keys
            .iter()
            .filter(|(_, scope, _)| scopes.contains(scope))
            .filter_map(|(argname, scope, _)| {
                let group = self.instance_at(*scope);
                if next.instance_at(*scope) != group {
                    return None;
                }
                let prev_val = self.param_value_repr(py, argname);
                let next_val = next.param_value_repr(py, argname);
                // Same value (still requested) → the cached instance stays
                // valid; only a changed or dropped value ends the group.
                if next_val.is_some() && next_val == prev_val {
                    return None;
                }
                Some((*scope, group, argname.clone(), prev_val.unwrap_or_default()))
            })
            .collect()
    }
}

/// --ignore / --ignore-glob filters, pruned during collection traversal
/// (an ignored directory is never walked).
#[derive(Default)]
pub struct CollectIgnores {
    /// Canonicalized --ignore paths; a directory ignores its whole tree.
    paths: Vec<PathBuf>,
    /// --ignore-glob patterns, fnmatch-style against the full path
    /// (upstream pytest_ignore_collect).
    globs: Vec<String>,
}

impl CollectIgnores {
    pub fn from_config(config: &crate::config::Config) -> Self {
        // Both the as-given and canonicalized forms: directory walks see
        // symlinked paths uncanonicalized, explicit args canonicalized.
        let mut paths = Vec::new();
        for value in config.get_values("ignore").unwrap_or_default() {
            let path = config.invocation_dir.join(value);
            if let Ok(canonical) = std::fs::canonicalize(&path)
                && canonical != path
            {
                paths.push(canonical);
            }
            paths.push(path);
        }
        Self {
            paths,
            globs: config
                .get_values("ignore-glob")
                .unwrap_or_default()
                .iter()
                .map(|value| value.to_string())
                .collect(),
        }
    }

    fn is_ignored(&self, path: &Path) -> bool {
        if self.paths.iter().any(|ignore| path.starts_with(ignore)) {
            return true;
        }
        if self.globs.is_empty() {
            return false;
        }
        let text = path.to_string_lossy();
        self.globs.iter().any(|glob| wildcard_match(glob, &text))
    }
}

/// Canonicalize a path up to (but not through) its first symlink component,
/// leaving that component and everything under it untouched. Resolves
/// platform path-normalization quirks (e.g. macOS's /tmp → /private/tmp)
/// without following symlinks the way `std::fs::canonicalize` does — matching
/// upstream's `resolve_collection_argument`, which uses plain
/// `os.path.abspath` and never resolves symlinks at all.
pub(crate) fn canonicalize_preserving_symlinks(path: &Path) -> PathBuf {
    let mut canonical_prefix = PathBuf::new();
    let mut rest = PathBuf::new();
    let mut past_symlink = false;
    for component in path.components() {
        if past_symlink {
            rest.push(component);
            continue;
        }
        canonical_prefix.push(component);
        if std::fs::symlink_metadata(&canonical_prefix)
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false)
        {
            past_symlink = true;
        }
    }
    if !past_symlink {
        return std::fs::canonicalize(&canonical_prefix).unwrap_or(canonical_prefix);
    }
    // canonical_prefix currently includes the symlink itself; canonicalize
    // only its parent, then re-append the symlink name and the untouched tail.
    let symlink_name = canonical_prefix
        .file_name()
        .map(PathBuf::from)
        .unwrap_or_default();
    let parent = canonical_prefix.parent().unwrap_or(&canonical_prefix);
    let canonical_parent = std::fs::canonicalize(parent).unwrap_or_else(|_| parent.to_path_buf());
    canonical_parent.join(symlink_name).join(rest)
}

/// Expand CLI path arguments into the ordered list of test files.
pub fn collect_test_files(
    invocation_dir: &Path,
    paths: &[String],
    collect_in_virtualenv: bool,
    python_files: &[String],
    norecursedirs: &[String],
    keep_duplicates: bool,
    ignores: &CollectIgnores,
) -> Result<(Vec<PathBuf>, Vec<String>), String> {
    let args: Vec<String> = if paths.is_empty() {
        vec![".".to_string()]
    } else {
        paths.to_vec()
    };

    struct ResolvedArg {
        path: PathBuf,
        has_parts: bool,
        is_dir: bool,
    }

    let mut resolved = Vec::with_capacity(args.len());
    let mut not_found_args = Vec::new();
    for arg in &args {
        // Node-id args select within a file; only the path part is
        // collected here (the engine filters items afterwards).
        let path_arg = arg.split("::").next().unwrap_or(arg);
        // Canonicalize only the prefix before the first symlink component
        // (e.g. /tmp → /private/tmp on macOS), preserving any symlink
        // component and everything under it as-given — so a nodeid built
        // through a symlinked dir (or a symlinked file) keeps the symlink's
        // name rather than resolving to its target, matching upstream's
        // resolve_collection_argument (plain os.path.abspath, no symlink
        // resolution at all).
        let raw = invocation_dir.join(path_arg);
        let path = canonicalize_preserving_symlinks(&raw);
        // Upstream UsageError wording, with the argument as the user gave it.
        // Defer: don't error here so that conftest loading and
        // pytest_configure have a chance to fire first (issue #143).
        let Ok(meta) = std::fs::metadata(&path) else {
            not_found_args.push(arg.clone());
            continue;
        };
        // --ignore applies to explicit args too (upstream
        // pytest_ignore_collect covers the whole collection tree).
        if ignores.is_ignored(&path) {
            continue;
        }
        resolved.push(ResolvedArg {
            path,
            has_parts: arg.contains("::"),
            is_dir: meta.is_dir(),
        });
    }

    // Mirror upstream's normalize_collection_arguments: when one arg's path
    // is a directory ancestor of (or equal to) another arg without node-id
    // parts, the descendant is redundant and dropped, keeping CLI order
    // among the survivors. --keep-duplicates bypasses this entirely.
    let survivors: Vec<&ResolvedArg> = if keep_duplicates {
        resolved.iter().collect()
    } else {
        let mut indexed: Vec<(usize, &ResolvedArg)> = resolved.iter().enumerate().collect();
        indexed.sort_by(|a, b| {
            a.1.path
                .cmp(&b.1.path)
                .then(a.1.has_parts.cmp(&b.1.has_parts))
        });
        let mut kept_indices = Vec::new();
        let mut last_kept: Option<&ResolvedArg> = None;
        for (idx, ra) in &indexed {
            let subsumed = match last_kept {
                Some(prev) if prev.path == ra.path => {
                    !prev.has_parts || prev.has_parts == ra.has_parts
                }
                Some(prev) => !prev.has_parts && ra.path.starts_with(&prev.path),
                None => false,
            };
            if subsumed {
                continue;
            }
            kept_indices.push(*idx);
            last_kept = Some(ra);
        }
        kept_indices.sort_unstable();
        kept_indices.into_iter().map(|i| &resolved[i]).collect()
    };

    let mut files = Vec::new();
    for ra in survivors {
        if ra.is_dir {
            // An explicitly given directory is collected even if it is a
            // virtualenv root; only recursion skips them.
            collect_dir(
                &ra.path,
                files.as_mut(),
                collect_in_virtualenv,
                python_files,
                norecursedirs,
                ignores,
                keep_duplicates,
            )?;
        } else if keep_duplicates || !files.contains(&ra.path) {
            // --keep-duplicates: a file given twice collects twice (pytest
            // keeps duplicated args).
            files.push(ra.path.clone());
        }
    }
    Ok((files, not_found_args))
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
    norecursedirs: &[String],
    ignores: &CollectIgnores,
    keep_duplicates: bool,
) -> Result<(), String> {
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .map_err(|e| format!("{}: {e}", dir.display()))?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .collect();
    entries.sort();
    for path in entries {
        if ignores.is_ignored(&path) {
            // --ignore/--ignore-glob prune the tree before any walk.
            continue;
        }
        if path.is_dir() {
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            // __pycache__ never holds source files; skipping it is just speed.
            if name != "__pycache__"
                && !matches_norecurse(&path, name, norecursedirs)
                && (collect_in_virtualenv || !in_venv(&path))
            {
                collect_dir(
                    &path,
                    files,
                    collect_in_virtualenv,
                    python_files,
                    norecursedirs,
                    ignores,
                    keep_duplicates,
                )?;
            }
        } else if is_test_file(&path, python_files)
            && path.is_file()
            && (keep_duplicates || !files.contains(&path))
        {
            // Overlapping arguments ("pytest a a/b") collect each file
            // once; is_file also drops broken symlinks.
            files.push(path);
        }
    }
    Ok(())
}

/// pytest's fnmatch_ex over norecursedirs: a bare pattern matches the
/// directory basename, a pattern with "/" matches the whole path (relative
/// patterns get a "*/" prefix).
fn matches_norecurse(path: &Path, name: &str, patterns: &[String]) -> bool {
    patterns.iter().any(|pattern| {
        if pattern.contains('/') {
            let full = path.to_string_lossy();
            if path.is_absolute() && !pattern.starts_with('/') {
                wildcard_match(&format!("*/{pattern}"), &full)
            } else {
                wildcard_match(pattern, &full)
            }
        } else {
            wildcard_match(pattern, name)
        }
    })
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
pub(crate) fn wildcard_match(pattern: &str, name: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let name: Vec<char> = name.chars().collect();
    let (mut p, mut n) = (0usize, 0usize);
    let mut star: Option<(usize, usize)> = None;
    while n < name.len() {
        if p < pattern.len()
            && pattern[p] != '*'
            && let Some(next_p) = match_token(&pattern, p, name[n])
        {
            p = next_p;
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

/// Match one fnmatch token (`?`, a `[seq]` / `[!seq]` character class, or a
/// literal) against `ch`. Returns the pattern index just past the token on
/// a match, or None. A class with no closing `]` is a literal `[`.
fn match_token(pattern: &[char], p: usize, ch: char) -> Option<usize> {
    match pattern[p] {
        '?' => Some(p + 1),
        '[' => {
            // A ']' immediately after '[' or '[!' is a literal member.
            let mut end = p + 1;
            let negated = pattern.get(end) == Some(&'!');
            if negated {
                end += 1;
            }
            let class_start = end;
            if pattern.get(end) == Some(&']') {
                end += 1;
            }
            while end < pattern.len() && pattern[end] != ']' {
                end += 1;
            }
            if end >= pattern.len() {
                // No closing bracket: treat '[' as a literal.
                return (pattern[p] == ch).then_some(p + 1);
            }
            let class = &pattern[class_start..end];
            let mut matched = false;
            let mut idx = 0;
            while idx < class.len() {
                if idx + 2 < class.len() && class[idx + 1] == '-' {
                    // A range like a-z (the '-' is literal at the edges).
                    if class[idx] <= ch && ch <= class[idx + 2] {
                        matched = true;
                    }
                    idx += 3;
                } else {
                    if class[idx] == ch {
                        matched = true;
                    }
                    idx += 1;
                }
            }
            (matched != negated).then_some(end + 1)
        }
        literal => (literal == ch).then_some(p + 1),
    }
}

/// pytest "prepend" import mode: walk up while __init__.py exists; the first
/// directory without one is the sys.path root, and the dotted module name is
/// the relative path from there.
pub fn module_name_for(path: &Path) -> (PathBuf, String) {
    let mut basedir = path.parent().unwrap_or(Path::new(".")).to_path_buf();
    let stem = path.file_stem().unwrap().to_string_lossy().to_string();
    // pkg/__init__.py imports as package "pkg", not "pkg.__init__".
    let mut parts = if stem == "__init__" {
        vec![]
    } else {
        vec![stem]
    };
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

/// `--import-mode` (upstream `_pytest.pathlib.ImportMode`): controls how
/// test/conftest files are turned into importable modules.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ImportMode {
    Prepend,
    Append,
    Importlib,
}

impl ImportMode {
    /// Reads `--import-mode` (default "prepend"); an unrecognized value
    /// falls back to prepend rather than erroring, matching upstream's
    /// lenient CLI choices handling elsewhere in pytest-rs.
    pub fn from_config(config: &crate::config::Config) -> Self {
        match config.get_value("import-mode") {
            Some("append") => ImportMode::Append,
            Some("importlib") => ImportMode::Importlib,
            _ => ImportMode::Prepend,
        }
    }
}

/// upstream `_pytest.pathlib.module_name_from_path`: a dotted module name
/// derived from the full path relative to `root`, used by `--import-mode
/// importlib` when the file does not belong to a package (no `__init__.py`
/// chain) — unlike `module_name_for`, this stays unique across sibling
/// directories with same-named files (the value doesn't depend on walking
/// up from the file, only on its full location).
pub fn module_name_from_path(path: &Path, root: &Path) -> String {
    let path = path.with_extension("");
    let parts: Vec<String> = match path.strip_prefix(root) {
        Ok(relative) => relative
            .components()
            .map(|c| c.as_os_str().to_string_lossy().to_string())
            .collect(),
        // No common root: use the full path parts except the first
        // ("/" or "C:\\" depending on platform).
        Err(_) => path
            .components()
            .skip(1)
            .map(|c| c.as_os_str().to_string_lossy().to_string())
            .collect(),
    };
    // A package's module name does not include the trailing __init__,
    // unless __init__.py sits directly at the root.
    let mut parts = parts;
    if parts.len() >= 2 && parts.last().map(String::as_str) == Some("__init__") {
        parts.pop();
    }
    parts
        .iter()
        .map(|p| p.replace('.', "_"))
        .collect::<Vec<_>>()
        .join(".")
}

/// Collect all Python files (including non-test files like __init__.py) for
/// --doctest-modules. Does not include files already covered by collect_test_files.
pub fn collect_all_python_files(
    invocation_dir: &Path,
    paths: &[String],
    collect_in_virtualenv: bool,
    already_collected: &[PathBuf],
) -> Vec<PathBuf> {
    collect_all_python_files_ext(
        invocation_dir,
        paths,
        collect_in_virtualenv,
        already_collected,
        false,
    )
}

/// `include_pyi` extends the sweep to `.pyi` stubs, for custom collectors
/// (pytest-mypy) whose `pytest_collect_file` handles `.pyi` files.
pub fn collect_all_python_files_ext(
    invocation_dir: &Path,
    paths: &[String],
    collect_in_virtualenv: bool,
    already_collected: &[PathBuf],
    include_pyi: bool,
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
            collect_all_py_dir(&path, &mut files, collect_in_virtualenv, include_pyi);
        } else if is_py_candidate(&path, include_pyi)
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

fn is_py_candidate(path: &Path, include_pyi: bool) -> bool {
    match path.extension().and_then(|e| e.to_str()) {
        Some("py") => true,
        Some("pyi") => include_pyi,
        _ => false,
    }
}

fn collect_all_py_dir(
    dir: &Path,
    files: &mut Vec<PathBuf>,
    collect_in_virtualenv: bool,
    include_pyi: bool,
) {
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
    let Ok(read_dir) = std::fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<_> = read_dir.filter_map(Result::ok).map(|e| e.path()).collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if !NORECURSE.contains(&name)
                && !name.starts_with('.')
                && (collect_in_virtualenv || !in_venv(&path))
            {
                collect_all_py_dir(&path, files, collect_in_virtualenv, include_pyi);
            }
        } else if is_py_candidate(&path, include_pyi) && path.is_file() && !files.contains(&path) {
            files.push(path);
        }
    }
}

/// Gather ALL files (any extension) from search paths for `pytest_collect_file` hooks.
pub fn collect_all_files(
    invocation_dir: &Path,
    paths: &[String],
    collect_in_virtualenv: bool,
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
            collect_all_files_dir(&path, &mut files, collect_in_virtualenv);
        } else if path.is_file() && !files.contains(&path) {
            files.push(path);
        }
    }
    files
}

fn collect_all_files_dir(dir: &Path, files: &mut Vec<PathBuf>, collect_in_virtualenv: bool) {
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
    let Ok(read_dir) = std::fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<_> = read_dir.filter_map(Result::ok).map(|e| e.path()).collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if !NORECURSE.contains(&name)
                && !name.starts_with('.')
                && (collect_in_virtualenv || !in_venv(&path))
            {
                collect_all_files_dir(&path, files, collect_in_virtualenv);
            }
        } else if path.is_file() && !files.contains(&path) {
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
    let Ok(read_dir) = std::fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<_> = read_dir.filter_map(Result::ok).map(|e| e.path()).collect();
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

/// Resolves each CLI collection argument to a canonicalized filesystem path
/// (stripping any `::nodeid` suffix), the same way `collect_test_files`
/// resolves its own `args`. Mirrors pytest's `session._initialpaths`: the
/// frozenset of resolved `CollectionArgument.path`s used as the fallback
/// anchor for nodeids of files collected from outside `rootdir` (see
/// `file_nodeid`'s `Err` branch). Non-existent arguments are dropped, like
/// upstream's `resolve_collection_argument`.
pub fn resolve_initial_paths(invocation_dir: &Path, paths: &[String]) -> Vec<PathBuf> {
    let args: Vec<&str> = if paths.is_empty() {
        vec!["."]
    } else {
        paths.iter().map(String::as_str).collect()
    };
    let mut initial = Vec::with_capacity(args.len());
    for arg in args {
        let path_arg = arg.split("::").next().unwrap_or(arg);
        let raw = invocation_dir.join(path_arg);
        let path = canonicalize_preserving_symlinks(&raw);
        if std::fs::metadata(&path).is_ok() && !initial.contains(&path) {
            initial.push(path);
        }
    }
    initial
}

/// Node id for a test file: path relative to rootdir with '/' separators.
///
/// When the file lives outside `rootdir`, mirrors pytest's
/// `_check_initialpaths_for_relpath` (`_pytest/nodes.py`): walk the file's
/// ancestor directories and, if one matches a resolved CLI collection
/// argument in `initial_paths`, return the path relative to that ancestor
/// instead of falling back to the absolute path.
pub fn file_nodeid(rootdir: &Path, path: &Path, initial_paths: &[PathBuf]) -> String {
    if let Ok(relative) = path.strip_prefix(rootdir) {
        return relative
            .components()
            .map(|c| c.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/");
    }
    if initial_paths.iter().any(|p| p == path) {
        return String::new();
    }
    for parent in path.ancestors().skip(1) {
        if initial_paths.iter().any(|p| p == parent)
            && let Ok(relative) = path.strip_prefix(parent)
        {
            return relative
                .components()
                .map(|c| c.as_os_str().to_string_lossy())
                .collect::<Vec<_>>()
                .join("/");
        }
    }
    path.to_string_lossy().replace('\\', "/")
}

/// Display path for a test file in the progress output, matching pytest's
/// `write_fspath_result`: `bestrelpath(invocation_dir, rootdir / nodeid_file_part)`.
///
/// Real pytest builds nodeids for outside-rootdir files relative to the initial
/// collection path (typically the invocation dir), so
/// `rootdir / nodeid_file_part` produces a path under the rootdir even though
/// the actual file lives elsewhere.  `bestrelpath(invocation_dir, ...)` then
/// gives a tidy relative display.  We replicate that by computing
/// `rootdir / strip_prefix(invocation_dir, path)` when the file is outside the
/// rootdir but inside the invocation dir, with a plain `bestrelpath` fallback.
pub fn display_file_path(rootdir: &Path, invocation_dir: &Path, path: &Path) -> String {
    // Fast path: file is inside rootdir — display is invocation-dir-relative.
    if path.starts_with(rootdir) {
        return file_nodeid(invocation_dir, path, &[]);
    }
    // File is outside rootdir: mimic pytest's write_fspath_result.
    // pytest nodeid for such a file = path.relative_to(initial_collection_path)
    // (usually the invocation dir).  Then display = bestrelpath(invocation_dir,
    // rootdir / nodeid_part).
    if let Ok(rel_to_inv) = path.strip_prefix(invocation_dir) {
        let virtual_path = rootdir.join(rel_to_inv);
        return bestrelpath(invocation_dir, &virtual_path);
    }
    // Last resort: plain relative path from invocation dir.
    bestrelpath(invocation_dir, path)
}

/// Rust equivalent of pytest's `bestrelpath(directory, dest)`: returns a
/// relative path string from `directory` to `dest`.  Falls back to the
/// absolute path string when the two share no common ancestor.
pub(crate) fn bestrelpath(directory: &Path, dest: &Path) -> String {
    // Find the longest common prefix component-by-component.
    let dir_parts: Vec<_> = directory.components().collect();
    let dest_parts: Vec<_> = dest.components().collect();
    let common_len = dir_parts
        .iter()
        .zip(dest_parts.iter())
        .take_while(|(a, b)| a == b)
        .count();
    if common_len == 0 {
        return dest.to_string_lossy().replace('\\', "/");
    }
    let up = dir_parts.len() - common_len;
    let down = &dest_parts[common_len..];
    let mut parts: Vec<std::borrow::Cow<str>> = Vec::new();
    for _ in 0..up {
        parts.push("..".into());
    }
    for c in down {
        parts.push(c.as_os_str().to_string_lossy());
    }
    if parts.is_empty() {
        ".".to_string()
    } else {
        parts.join("/")
    }
}

/// Upstream's `Config.cwd_relative_nodeid`: nodeids are always rootdir-
/// relative internally (matching, `-k`, parametrize IDs, JUnit XML — none of
/// that changes here), but a nodeid printed to the terminal is re-relativized
/// against the invocation dir when it differs from rootdir (e.g. an explicit
/// `--rootdir=subdir` run from its parent). Display-only; never touches
/// `TestItem.nodeid`/`file_nodeid`.
pub fn cwd_relative_nodeid(rootdir: &Path, invocation_dir: &Path, nodeid: &str) -> String {
    if invocation_dir == rootdir {
        return nodeid.to_string();
    }
    let (path_part, rest) = nodeid.split_once("::").unwrap_or((nodeid, ""));
    let fullpath = rootdir.join(path_part);
    let relative_path = bestrelpath(invocation_dir, &fullpath);
    if rest.is_empty() {
        relative_path
    } else {
        format!("{relative_path}::{rest}")
    }
}
