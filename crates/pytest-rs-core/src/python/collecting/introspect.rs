//! Python-side collection: modules, classes, TestCases, doctests, parametrize.

#[allow(unused_imports)]
use super::super::*;
use std::path::PathBuf;

/// Test-discovery name filters from the `python_classes` / `python_functions`
/// ini options. A name matches when it starts with a pattern, or (when the
/// pattern contains glob chars) fnmatch-globs it — mirroring pytest's
/// `PyCollector._matches_prefix_or_glob_option`.
pub struct NameFilters {
    pub classes: Vec<String>,
    pub functions: Vec<String>,
    /// Builtin attribute names ignored before pattern matching (pytest's
    /// IGNORED_ATTRIBUTES); keeps `python_*=*` from collecting dunders.
    pub ignored: std::collections::HashSet<String>,
}

impl NameFilters {
    pub fn from_config(py: Python<'_>, config: &crate::config::Config) -> Self {
        let ignored = py
            .import("pytest._pycollect")
            .and_then(|m| m.getattr("ignored_attributes"))
            .and_then(|f| f.call0())
            .and_then(|v| v.extract::<Vec<String>>())
            .unwrap_or_default()
            .into_iter()
            .collect();
        NameFilters {
            classes: config.python_classes_patterns(),
            functions: config.python_functions_patterns(),
            ignored,
        }
    }

    /// True when `name` is a builtin attribute pytest ignores before any
    /// name-pattern matching (so it's never collected or warned about).
    pub fn is_ignored(&self, name: &str) -> bool {
        self.ignored.contains(name)
    }

    pub fn matches_class(&self, name: &str) -> bool {
        Self::matches(&self.classes, name)
    }

    pub fn matches_function(&self, name: &str) -> bool {
        Self::matches(&self.functions, name)
    }

    fn matches(patterns: &[String], name: &str) -> bool {
        patterns.iter().any(|pattern| {
            name.starts_with(pattern)
                || (pattern.contains(['*', '?', '['])
                    && crate::collect::wildcard_match(pattern, name))
        })
    }
}

/// The 1-based first line of a callable's definition (0 if unknown).
pub(crate) fn first_lineno(py: Python<'_>, func: &Bound<'_, PyAny>) -> u32 {
    let _ = py;
    func.getattr("__code__")
        .and_then(|code| code.getattr("co_firstlineno"))
        .and_then(|line| line.extract::<u32>())
        .unwrap_or(0)
}

/// Names of the fixture-requesting parameters of a Python callable, in
/// order: positional/keyword params without defaults (defaulted params and
/// *args/**kwargs are not fixture requests, matching pytest).
pub fn param_names(py: Python<'_>, func: &Bound<'_, PyAny>) -> PyResult<Vec<String>> {
    param_names_inner(py, func).map(|(names, _)| names)
}

/// Like `param_names` but also reports whether any positional-only
/// parameter exists (pytest skips the first-arg strip when it does).
pub fn param_names_with_positional_only(
    py: Python<'_>,
    func: &Bound<'_, PyAny>,
) -> PyResult<(Vec<String>, bool)> {
    param_names_inner(py, func)
}

fn param_names_inner(py: Python<'_>, func: &Bound<'_, PyAny>) -> PyResult<(Vec<String>, bool)> {
    let inspect = py.import("inspect")?;
    let signature = inspect.getattr("signature")?.call1((func,))?;
    let parameters = signature.getattr("parameters")?;
    let empty = inspect.getattr("Parameter")?.getattr("empty")?;
    let mut names = Vec::new();
    let mut has_positional_only = false;
    for value in parameters.call_method0("values")?.try_iter()? {
        let parameter = value?;
        let kind = parameter.getattr("kind")?;
        let kind_name: String = kind.getattr("name")?.extract()?;
        if kind_name == "POSITIONAL_ONLY" {
            has_positional_only = true;
            continue;
        }
        if kind_name != "POSITIONAL_OR_KEYWORD" && kind_name != "KEYWORD_ONLY" {
            continue;
        }
        if !parameter.getattr("default")?.is(&empty) {
            continue;
        }
        names.push(parameter.getattr("name")?.extract()?);
    }
    Ok((names, has_positional_only))
}

/// pytest compat.num_mock_patch_args: how many leading parameters are
/// injected by stacked @unittest.mock.patch decorators (their `patchings`
/// entries with no attribute_name and new=DEFAULT). Those are mock-filled
/// positionally at call time, not fixture requests.
pub fn num_mock_patch_args(py: Python<'_>, func: &Bound<'_, PyAny>) -> usize {
    let Ok(patchings) = func.getattr("patchings") else {
        return 0;
    };
    let Ok(iter) = patchings.try_iter() else {
        return 0;
    };
    // Both the stdlib and the rolling-backport `mock` define the sentinel;
    // like pytest, only consult already-imported modules (sys.modules).
    let modules = py.import("sys").and_then(|sys| sys.getattr("modules")).ok();
    let sentinels: Vec<Bound<'_, PyAny>> = ["unittest.mock", "mock"]
        .iter()
        .filter_map(|name| {
            modules
                .as_ref()?
                .get_item(name)
                .ok()?
                .getattr("DEFAULT")
                .ok()
        })
        .collect();
    iter.flatten()
        .filter(|p| {
            let no_attribute_name = p
                .getattr("attribute_name")
                .map(|v| !v.is_truthy().unwrap_or(true))
                .unwrap_or(false);
            let new_is_default = p
                .getattr("new")
                .map(|new| sentinels.iter().any(|s| new.is(s)))
                .unwrap_or(false);
            no_attribute_name && new_is_default
        })
        .count()
}

pub struct AsyncFlags {
    pub is_coroutine: bool,
    pub is_generator: bool,
    pub is_async_gen: bool,
}

pub fn async_flags(py: Python<'_>, func: &Bound<'_, PyAny>) -> PyResult<AsyncFlags> {
    let inspect = py.import("inspect")?;
    Ok(AsyncFlags {
        is_coroutine: inspect
            .getattr("iscoroutinefunction")?
            .call1((func,))?
            .extract()?,
        is_generator: inspect
            .getattr("isgeneratorfunction")?
            .call1((func,))?
            .extract()?,
        is_async_gen: inspect
            .getattr("isasyncgenfunction")?
            .call1((func,))?
            .extract()?,
    })
}

/// Import one test module and introspect it: append discovered test items
/// Resolve a dotted module name to a filesystem path via `importlib.util.find_spec`.
/// Returns `Some(path)` for a module file or package directory, `None` if not found.
/// Handles regular packages (`__init__.py`) and namespace packages (PEP 420).
pub fn resolve_pyarg(py: Python<'_>, module_name: &str) -> Option<PathBuf> {
    let find_spec = py
        .import("importlib.util")
        .ok()?
        .getattr("find_spec")
        .ok()?;
    let spec = find_spec.call1((module_name,)).ok()?;
    if spec.is_none() {
        return None;
    }
    let sub_locs = spec.getattr("submodule_search_locations").ok()?;
    if sub_locs.is_none() || sub_locs.len().unwrap_or(0) == 0 {
        // Simple module (not a package).
        let origin = spec.getattr("origin").ok()?;
        if origin.is_none() {
            return None;
        }
        return origin.extract::<String>().ok().map(PathBuf::from);
    }
    // Package: try origin first (regular package with __init__.py),
    // fall back to submodule_search_locations[0] (namespace package).
    let origin = spec.getattr("origin").ok()?;
    if !origin.is_none() {
        let origin_str: String = origin.extract().ok()?;
        return PathBuf::from(&origin_str).parent().map(|p| p.to_path_buf());
    }
    // Namespace package: no __init__.py, use search location directly.
    let loc: String = sub_locs.get_item(0).ok()?.extract().ok()?;
    Some(PathBuf::from(loc))
}

/// The `--collect-only` tree's "import root" for one resolved `--pyargs`
/// argument: the topmost ancestor of `argpath` whose name chain still spells
/// out the tail of `module_name` (e.g. `pkg.sub.test_it` matches ancestors
/// named "sub" then "pkg"). Mirrors upstream's `Session.collect()` (the loop
/// building the `paths` list from `argpath.parents`, `main.py`) — that
/// anchor is the only node upstream parents directly onto the Session,
/// skipping any plain filesystem ancestors above it (e.g. a venv/site-packages
/// prefix) entirely, which is why the tree display only ever shows one
/// combined label for that span instead of one label per directory.
pub fn pyargs_anchor(argpath: &Path, module_name: &str) -> PathBuf {
    let parts: Vec<&str> = module_name.split('.').collect();
    let mut anchor = argpath.to_path_buf();
    for (i, parent) in (2..).zip(argpath.ancestors().skip(1)) {
        if i > parts.len() {
            break;
        }
        let stem = parent
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        if stem != parts[parts.len() - i] {
            break;
        }
        anchor = parent.to_path_buf();
    }
    anchor
}
