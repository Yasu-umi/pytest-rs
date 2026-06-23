//! Python-side collection: modules, classes, TestCases, doctests, parametrize.

#[allow(unused_imports)]
use super::*;
use crate::collect::{MarkData, TestItem, file_nodeid, module_name_for};
use crate::fixture::FixtureRegistry;
use pyo3::types::{PyList, PyModule};
use std::path::{Path, PathBuf};

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

/// and fixture definitions (objects carrying recorded shim metadata).
/// True when a `pytest_collect_directory` hook exists in conftest hooks or on a
/// pluginmanager plugin. The default implementation in `_pytest.python` always
/// exists; this checks for CONFTEST overrides that can filter/replace dirs.
pub fn has_collect_directory_hook(_py: Python<'_>, hooks: &[crate::session::PyHook]) -> bool {
    hooks.iter().any(|h| h.name == "pytest_collect_directory")
}

/// Result of firing `pytest_collect_directory`.
pub enum CollectDirResult {
    /// Hook returned None: skip the directory entirely.
    Skip,
    /// Hook returned the default Dir or a custom collector: let Rust handle
    /// the files in this directory (custom directory collection requires
    /// a Python-side `pytest_collect_file` implementation which is not yet
    /// available).
    Default,
}

/// Fire the `pytest_collect_directory` hook via the pluginmanager relay.
pub fn call_collect_directory_hook(py: Python<'_>, dir: &Path, rootdir: &Path) -> CollectDirResult {
    let pm = match py
        .import("pytest._pluginmanager")
        .and_then(|m| m.getattr("pluginmanager"))
    {
        Ok(pm) => pm,
        Err(_) => return CollectDirResult::Default,
    };
    let hook_relay = match pm
        .getattr("hook")
        .and_then(|h| h.getattr("pytest_collect_directory"))
    {
        Ok(h) => h,
        Err(_) => return CollectDirResult::Default,
    };
    let pathlib = match py.import("pathlib").and_then(|m| m.getattr("Path")) {
        Ok(p) => p,
        Err(_) => return CollectDirResult::Default,
    };
    let py_path = match pathlib.call1((dir.to_string_lossy().as_ref(),)) {
        Ok(p) => p,
        Err(_) => return CollectDirResult::Default,
    };
    let config = crate::python::proxies::existing_py_config(py).map(|c| c.into_bound(py));
    let node_mod = match py.import("pytest._node") {
        Ok(m) => m,
        Err(_) => return CollectDirResult::Default,
    };
    let collector_cls = match node_mod.getattr("Collector") {
        Ok(c) => c,
        Err(_) => return CollectDirResult::Default,
    };
    let parent = {
        let kw = pyo3::types::PyDict::new(py);
        let _ = kw.set_item("config", config.as_ref().map(|c| c.as_any()));
        let root_path = pathlib.call1((rootdir.to_string_lossy().as_ref(),)).ok();
        let _ = kw.set_item("path", root_path.as_ref());
        let _ = kw.set_item("nodeid", "");
        let _ = kw.set_item("name", "");
        let session_proxy = config.as_ref().and_then(|c| {
            node_mod
                .getattr("_NodeSession")
                .ok()?
                .call1((c.as_any(),))
                .ok()
        });
        let _ = kw.set_item("session", session_proxy.as_ref());
        match collector_cls.call((), Some(&kw)) {
            Ok(p) => p,
            Err(_) => return CollectDirResult::Default,
        }
    };
    let kwargs = pyo3::types::PyDict::new(py);
    let _ = kwargs.set_item("path", &py_path);
    let _ = kwargs.set_item("parent", &parent);
    let result = match hook_relay.call((), Some(&kwargs)) {
        Ok(r) => r,
        Err(_) => return CollectDirResult::Default,
    };
    if result.is_none() {
        return CollectDirResult::Skip;
    }
    CollectDirResult::Default
}

/// True when a `pytest_pycollect_makemodule` hook exists in conftest hooks.
/// The default has no override; this checks for CONFTEST/plugin overrides that
/// can replace the Module collector with a custom subclass.
pub fn has_pycollect_makemodule_hook(_py: Python<'_>, hooks: &[crate::session::PyHook]) -> bool {
    hooks
        .iter()
        .any(|h| h.name == "pytest_pycollect_makemodule")
}

/// Fire `pytest_pycollect_makemodule(module_path, parent)` via the pluginmanager
/// relay (firstresult). Returns the returned collector node's class name when a
/// custom node is produced and it differs from the default "Module", so the
/// `--collect-only` tree can render e.g. `<MyModule xyz>`.
pub fn call_pycollect_makemodule_hook(
    py: Python<'_>,
    path: &Path,
    rootdir: &Path,
    module: &Bound<'_, PyAny>,
) -> Option<String> {
    let pm = py
        .import("pytest._pluginmanager")
        .and_then(|m| m.getattr("pluginmanager"))
        .ok()?;
    let hook_relay = pm
        .getattr("hook")
        .and_then(|h| h.getattr("pytest_pycollect_makemodule"))
        .ok()?;
    let pathlib = py.import("pathlib").and_then(|m| m.getattr("Path")).ok()?;
    let module_path = pathlib.call1((path.to_string_lossy().as_ref(),)).ok()?;
    let config = crate::python::proxies::existing_py_config(py).map(|c| c.into_bound(py));
    let node_mod = py.import("pytest._node").ok()?;
    let collector_cls = node_mod.getattr("Collector").ok()?;
    let parent = {
        let kw = pyo3::types::PyDict::new(py);
        let _ = kw.set_item("config", config.as_ref().map(|c| c.as_any()));
        let root_path = pathlib.call1((rootdir.to_string_lossy().as_ref(),)).ok();
        let _ = kw.set_item("path", root_path.as_ref());
        let _ = kw.set_item("nodeid", "");
        let _ = kw.set_item("name", "");
        let session_proxy = config.as_ref().and_then(|c| {
            node_mod
                .getattr("_NodeSession")
                .ok()?
                .call1((c.as_any(),))
                .ok()
        });
        let _ = kw.set_item("session", session_proxy.as_ref());
        let collector = collector_cls.call((), Some(&kw)).ok()?;
        // The default makemodule impl reads `parent._rs_module` so the live
        // Module node it returns wraps the real imported module — a conftest
        // hookwrapper can then mutate `mod.obj` (issue #205) on that module.
        let _ = collector.setattr("_rs_module", module);
        collector
    };
    let kwargs = pyo3::types::PyDict::new(py);
    let _ = kwargs.set_item("module_path", &module_path);
    let _ = kwargs.set_item("parent", &parent);
    let result = hook_relay.call((), Some(&kwargs)).ok()?;
    if result.is_none() {
        return None;
    }
    // The core default node carries `_rs_default_makemodule`; treat it (and a
    // plain pytest.Module) as the default collector so no custom label is set.
    let is_default = result
        .getattr("_rs_default_makemodule")
        .map(|v| v.is_truthy().unwrap_or(false))
        .unwrap_or(false);
    if is_default {
        return None;
    }
    let class_name: String = result
        .getattr("__class__")
        .and_then(|c| c.getattr("__name__"))
        .and_then(|n| n.extract())
        .ok()?;
    if class_name == "Module" {
        None
    } else {
        Some(class_name)
    }
}

/// True when a `pytest_pycollect_makeitem` hook is registered (conftest/plugin),
/// so introspection should consult it for custom function/item nodes.
pub fn has_pycollect_makeitem_hook(_py: Python<'_>, hooks: &[crate::session::PyHook]) -> bool {
    hooks.iter().any(|h| h.name == "pytest_pycollect_makeitem")
}

/// Fire `pytest_pycollect_makeitem(collector, name, obj)` for one namespace
/// member via the pluginmanager relay (firstresult). When a conftest returns a
/// custom node (or list of nodes) the result is `Some(vec![(class_name,
/// node_name)])` so the engine collects each as a leaf item rendered with the
/// custom class label (e.g. `<MyFunction some>`). `None` means no plugin claimed
/// this member, so the default Rust collection path applies.
type MakeItemResult = Vec<(String, String, Option<Py<PyAny>>)>;

#[allow(clippy::type_complexity)]
pub fn fire_pycollect_makeitem(
    py: Python<'_>,
    nodeid_base: &str,
    path: &Path,
    name: &str,
    obj: &Bound<'_, PyAny>,
    is_test_func: bool,
) -> Option<MakeItemResult> {
    // Delegate to pytest._node.fire_makeitem_for_function which:
    // 1. Builds a plain Function node as the "inner result" (only when
    //    is_test_func=True so that wrapper hooks receive a node to attach
    //    attributes to; non-test members get None, so ``if result:`` guards
    //    in wrapper hooks correctly skip them)
    // 2. Runs wrapper hooks so they receive the node (and can attach attrs)
    // 3. Runs firstresult plain impls that may return a custom node/class
    // This mirrors what pytest's Module.collect() does via pluggy.
    let nodeid = format!("{nodeid_base}::{name}");
    let result = py
        .import("pytest._node")
        .and_then(|m| m.getattr("fire_makeitem_for_function"))
        .and_then(|f| {
            f.call1((
                nodeid.as_str(),
                name,
                obj,
                path.to_string_lossy().as_ref(),
                0i32,
                is_test_func,
            ))
        })
        .ok()?;
    if result.is_none() {
        return None;
    }
    // Result may be a single node or a list/tuple of nodes.
    let nodes: Vec<Bound<'_, PyAny>> =
        if result.hasattr("__iter__").unwrap_or(false) && result.try_iter().is_ok() {
            result.try_iter().ok()?.collect::<PyResult<_>>().ok()?
        } else {
            vec![result]
        };
    let mut out = Vec::new();
    for node in nodes {
        if node.is_none() {
            continue;
        }
        let class_name: String = node
            .getattr("__class__")
            .and_then(|c| c.getattr("__name__"))
            .and_then(|n| n.extract())
            .unwrap_or_else(|_| "Function".to_string());
        let node_name: String = node
            .getattr("name")
            .and_then(|n| n.extract())
            .unwrap_or_else(|_| name.to_string());
        // Preserve the Python node object so that custom subclasses (with
        // overridden reportinfo, extra attributes set by wrapper hooks, etc.)
        // are used as-is in make_py_node instead of being reconstructed.
        let py_node = Some(node.unbind());
        out.push((class_name, node_name, py_node));
    }
    if out.is_empty() { None } else { Some(out) }
}

/// Custom collectors: fire `pytest_collect_file(file_path, parent)` for each
/// candidate file; a plugin (pytest-ruff/pytest-mypy) may return a
/// `pytest.File` whose `.collect()` yields `pytest.Item`s. Each item becomes a
/// TestItem whose `func` is the item object itself (run via item.runtest()).
/// True when a pytest_collect_file hook exists (module-level in py_hooks, or
/// on a pluginmanager-registered plugin like pytest-mypy's MypyCollectionPlugin).
pub fn has_collect_file_hook(py: Python<'_>, hooks: &[crate::session::PyHook]) -> bool {
    if hooks.iter().any(|h| h.name == "pytest_collect_file") {
        return true;
    }
    py.import("pytest._pluginmanager")
        .and_then(|m| m.getattr("pluginmanager"))
        .and_then(|pm| {
            let plugins = pm.getattr("_plugins")?;
            for plugin in plugins.try_iter()? {
                if plugin?.hasattr("pytest_collect_file")? {
                    return Ok(true);
                }
            }
            Ok(false)
        })
        .unwrap_or(false)
}

/// Result of custom file collection.
pub struct CustomCollectResult {
    pub skipped: Vec<(PathBuf, String)>,
    pub errors: Vec<(PathBuf, String)>,
}

/// Collect items via pytest_collect_file hooks.
pub fn collect_custom_files(
    py: Python<'_>,
    rootdir: &Path,
    files: &[PathBuf],
    _hooks: &[crate::session::PyHook],
    items: &mut Vec<TestItem>,
) -> PyResult<CustomCollectResult> {
    let mut skipped: Vec<(PathBuf, String)> = Vec::new();
    let mut collect_errors: Vec<(PathBuf, String)> = Vec::new();
    let Some(config) = crate::python::proxies::existing_py_config(py) else {
        return Ok(CustomCollectResult {
            skipped,
            errors: collect_errors,
        });
    };
    let config = config.bind(py);
    // pytest_collect_file impls live on the shim pluginmanager (autoloaded
    // plugin modules + objects registered at configure, e.g. pytest-mypy);
    // the hook relay reaches them all.
    let collect_file = py
        .import("pytest._pluginmanager")?
        .getattr("pluginmanager")?
        .getattr("hook")?
        .getattr("pytest_collect_file")?;
    let pathlib = py.import("pathlib")?.getattr("Path")?;
    let node_mod = py.import("pytest._node")?;
    let collector_cls = node_mod.getattr("Collector")?;
    // A session stand-in with .config (plugins read parent.session.config).
    let session = node_mod.getattr("_NodeSession")?.call1((&config,))?;
    // Custom collectors (pytest-mypy) inspect session.items mid-collection to
    // decide what to yield; start from a clean slate and publish each yielded
    // item so later files see their siblings, matching real pytest's
    // incremental `self.items.extend(self.genitems(node))`.
    node_mod.call_method0("reset_collection_items")?;
    let publish_item = node_mod.getattr("publish_collection_item")?;
    for file in files {
        let file_path = pathlib.call1((file.to_string_lossy().as_ref(),))?;
        let parent = collector_cls.call(
            (),
            Some(&{
                let kw = pyo3::types::PyDict::new(py);
                kw.set_item("config", config)?;
                kw.set_item("session", &session)?;
                kw.set_item(
                    "path",
                    pathlib.call1((rootdir.to_string_lossy().as_ref(),))?,
                )?;
                kw.set_item("nodeid", "")?;
                kw.set_item("name", "")?;
                kw
            }),
        )?;
        // The relay returns a list of every plugin's result (collector|None).
        let kwargs = pyo3::types::PyDict::new(py);
        kwargs.set_item("file_path", &file_path)?;
        kwargs.set_item("parent", &parent)?;
        let results = match collect_file.call((), Some(&kwargs)) {
            Ok(r) => r,
            // pytest.skip() in pytest_collect_file means "skip this file".
            Err(ref err)
                if err
                    .get_type(py)
                    .name()
                    .map(|n| n == "Skipped")
                    .unwrap_or(false) =>
            {
                let reason = err
                    .value(py)
                    .getattr("msg")
                    .and_then(|m| m.extract::<String>())
                    .unwrap_or_else(|_| "Skipped".to_string());
                skipped.push((file.clone(), reason));
                continue;
            }
            Err(e) => return Err(e),
        };
        let results_list: Vec<Bound<'_, PyAny>> = if results.is_none() {
            Vec::new()
        } else {
            results.try_iter()?.collect::<PyResult<_>>()?
        };
        for collector in results_list {
            if collector.is_none() {
                continue;
            }
            let collector_class: String = collector
                .getattr("__class__")
                .and_then(|c| c.getattr("__name__"))
                .and_then(|n| n.extract())
                .unwrap_or_else(|_| "Module".to_string());
            // Update already-collected items for this file to use the custom
            // collector class (e.g. MyModule replacing the default Module).
            let pre_existing: std::collections::HashSet<String> = items
                .iter_mut()
                .filter_map(|it| {
                    if it.path == *file {
                        it.collector_class = collector_class.clone();
                        Some(it.nodeid.clone())
                    } else {
                        None
                    }
                })
                .collect();
            // If the hook returned a bare Item (not a Collector), treat it
            // directly as a single leaf item without calling .collect().
            let item_iter: Box<dyn Iterator<Item = PyResult<Bound<'_, PyAny>>>> = if collector
                .hasattr("collect")?
            {
                match collector.call_method0("collect") {
                    Ok(iter) => Box::new(iter.try_iter()?),
                    Err(err) => {
                        // Build ExceptionInfo and call repr_failure for custom formatting.
                        let longrepr = (|| -> PyResult<String> {
                            let excinfo_cls =
                                py.import("_pytest._code")?.getattr("ExceptionInfo")?;
                            let exc_value = err.value(py);
                            let ei = excinfo_cls.call_method1("from_exception", (exc_value,))?;
                            let repr = collector.call_method1("repr_failure", (&ei,))?;
                            repr.str()?.extract()
                        })()
                        .unwrap_or_else(|_| format_exception(py, &err));
                        collect_errors.push((file.clone(), longrepr));
                        continue;
                    }
                }
            } else {
                Box::new(std::iter::once(Ok(collector.clone())))
            };
            for item_obj in item_iter {
                let item_obj = item_obj?;
                // Publish to session.items immediately so a later file's
                // collect() sees this item (pytest-mypy's one-per-session
                // MypyStatusItem check).
                publish_item.call1((&item_obj,))?;
                let nodeid: String = item_obj.getattr("nodeid")?.extract()?;
                if pre_existing.contains(&nodeid) {
                    continue;
                }
                let name: String = item_obj.getattr("name")?.extract()?;
                let mut marks = Vec::new();
                if let Ok(own) = item_obj.getattr("own_markers") {
                    for mark in own.try_iter()? {
                        let mark = mark?;
                        marks.push(MarkData {
                            name: mark.getattr("name")?.extract()?,
                            obj: mark.unbind(),
                        });
                    }
                }
                items.push(TestItem {
                    nodeid,
                    path: file.clone(),
                    module_name: String::new(),
                    func_name: name,
                    func: item_obj.unbind(),
                    cls: None,
                    is_coroutine: false,
                    is_doctest: false,
                    fixture_names: Vec::new(),
                    extra_fixture_names: Vec::new(),
                    marks,
                    callspec: Vec::new(),
                    fixture_params: Vec::new(),
                    lineno: 0,
                    collector_class: collector_class.clone(),
                    func_class: String::new(),
                    py_node: None,
                    max_param_scope: crate::fixture::Scope::Function,
                    scope_sort_keys: Vec::new(),
                });
            }
        }
    }
    Ok(CustomCollectResult {
        skipped,
        errors: collect_errors,
    })
}

/// Mirror pytest's import_path ImportPathMismatchError: after importing a test
/// module by dotted name, the cached module's `__file__` must point at the file
/// we are collecting. A mismatch means two test files share a basename, which
/// pytest reports as a collection error rather than silently re-collecting the
/// first one. Reference: _pytest/pathlib.py:import_path / python.py:importtestmodule.
fn check_import_path_mismatch(
    py: Python<'_>,
    module: &Bound<'_, PyAny>,
    module_name: &str,
    path: &Path,
) -> PyResult<()> {
    // __init__.py packages are exempt, as is PY_IGNORE_IMPORTMISMATCH=1.
    if path.file_name().and_then(|n| n.to_str()) == Some("__init__.py") {
        return Ok(());
    }
    if std::env::var("PY_IGNORE_IMPORTMISMATCH").as_deref() == Ok("1") {
        return Ok(());
    }
    let module_file: Option<String> = module
        .getattr("__file__")
        .ok()
        .and_then(|f| f.extract::<String>().ok());
    // Normalize like pytest: .pyc/.pyo -> source, and a package's __init__.py
    // collapses to its directory before comparison.
    let normalized = module_file.as_ref().map(|mf| {
        let mut mf = mf.clone();
        if mf.ends_with(".pyc") || mf.ends_with(".pyo") {
            mf.pop();
        }
        let init_suffix = format!("{}__init__.py", std::path::MAIN_SEPARATOR);
        if let Some(stripped) = mf.strip_suffix(&init_suffix) {
            mf = stripped.to_string();
        }
        mf
    });
    let is_same = match &normalized {
        // os.path.samefile(path, module_file); a missing file is not the same.
        Some(mf) => py
            .import("os")
            .and_then(|os| os.getattr("path"))
            .and_then(|p| p.call_method1("samefile", (path, mf.as_str())))
            .and_then(|r| r.extract::<bool>())
            .unwrap_or(false),
        None => false,
    };
    if is_same {
        return Ok(());
    }
    // ImportPathMismatchError carries the normalized __file__ in its args.
    let message = format!(
        "import file mismatch:\n\
         imported module '{module_name}' has this __file__ attribute:\n  \
         {}\n\
         which is not the same as the test file we want to collect:\n  \
         {}\n\
         HINT: remove __pycache__ / .pyc files and/or use a unique basename for your test file modules",
        normalized.unwrap_or_default(),
        path.display(),
    );
    Err(collect_error(py, &message))
}

pub fn collect_module(
    py: Python<'_>,
    rootdir: &Path,
    path: &Path,
    items: &mut Vec<TestItem>,
    registry: &mut FixtureRegistry,
    hooks: &mut Vec<crate::session::PyHook>,
    filters: &NameFilters,
) -> PyResult<()> {
    let (basedir, module_name) = module_name_for(path);
    sys_path_prepend(py, &basedir)?;
    let module = py.import(module_name.as_str())?;
    // pytest's import_path raises ImportPathMismatchError when a module of the
    // same dotted name was already imported from a different file (e.g. two
    // test files sharing a basename in different dirs). We import by name and
    // get the cached module back, so mirror that check explicitly.
    check_import_path_mismatch(py, &module, &module_name, path)?;
    let nodeid_base = file_nodeid(rootdir, path);

    register_pytest_plugins(py, &module, registry, hooks)?;
    // Plugin/conftest pytest_generate_tests impls (e.g. pytest-repeat) run on
    // the metafunc alongside any module-level one.
    let extra_generate_hooks: Vec<Py<PyAny>> = hooks
        .iter()
        .filter(|hook| hook.name == "pytest_generate_tests")
        .map(|hook| hook.func.clone_ref(py))
        .collect();
    // pytest_pycollect_makemodule: a conftest may return a custom Module
    // subclass (e.g. `MyModule.from_parent(...)`) for this file. We honor the
    // returned node's class name for the --collect-only tree label.
    let custom_module_class = if has_pycollect_makemodule_hook(py, hooks) {
        call_pycollect_makemodule_hook(py, path, rootdir, module.as_any())
    } else {
        None
    };
    let makeitem_hook = has_pycollect_makeitem_hook(py, hooks);
    let module_items_start = items.len();
    introspect_namespace(
        py,
        &module,
        &nodeid_base,
        &module_name,
        path,
        items,
        registry,
        &extra_generate_hooks,
        makeitem_hook,
        filters,
    )?;
    if let Some(class_name) = custom_module_class {
        for item in items.iter_mut().skip(module_items_start) {
            item.collector_class = class_name.clone();
        }
    }
    Ok(())
}

/// Collect doctest items from an already-imported Python module.
/// Returns items appended to `items`; `py_config` is the PyConfig proxy.
pub fn collect_doctests_from_module(
    py: Python<'_>,
    rootdir: &Path,
    path: &Path,
    py_config: &Py<PyAny>,
    items: &mut Vec<TestItem>,
) -> PyResult<()> {
    let (basedir, module_name) = module_name_for(path);
    sys_path_prepend(py, &basedir)?;
    let nodeid_base = file_nodeid(rootdir, path);
    let doctest_mod = py.import("_pytest.doctest")?;
    let results = doctest_mod.getattr("collect_module_doctests")?.call1((
        module_name.as_str(),
        path.to_string_lossy().as_ref(),
        nodeid_base.as_str(),
        py_config.bind(py),
    ))?;
    for item in results.try_iter()? {
        let tuple = item?;
        let nodeid: String = tuple.get_item(0)?.extract()?;
        let func: Py<PyAny> = tuple.get_item(1)?.extract()?;
        let lineno: u32 = tuple.get_item(2)?.extract()?;
        // Derive func_name from the last "::" component of the nodeid.
        let func_name = nodeid.rsplit("::").next().unwrap_or(&nodeid).to_string();
        items.push(TestItem {
            nodeid,
            path: path.to_path_buf(),
            module_name: module_name.clone(),
            func_name,
            func,
            cls: None,
            is_coroutine: false,
            is_doctest: true,
            fixture_names: vec!["doctest_namespace".to_string(), "request".to_string()],
            extra_fixture_names: vec![],
            marks: vec![],
            callspec: vec![],
            fixture_params: vec![],
            lineno,
            collector_class: String::new(),
            func_class: String::new(),
            py_node: None,
            max_param_scope: crate::fixture::Scope::Function,
            scope_sort_keys: Vec::new(),
        });
    }
    Ok(())
}

/// Collect doctest items from a text file (e.g. `*.rst`).
pub fn collect_doctests_from_textfile(
    py: Python<'_>,
    rootdir: &Path,
    path: &Path,
    py_config: &Py<PyAny>,
    items: &mut Vec<TestItem>,
) -> PyResult<()> {
    let nodeid_base = file_nodeid(rootdir, path);
    let doctest_mod = py.import("_pytest.doctest")?;
    let results = doctest_mod.getattr("collect_textfile_doctests")?.call1((
        path.to_string_lossy().as_ref(),
        nodeid_base.as_str(),
        py_config.bind(py),
    ))?;
    for item in results.try_iter()? {
        let tuple = item?;
        let nodeid: String = tuple.get_item(0)?.extract()?;
        let func: Py<PyAny> = tuple.get_item(1)?.extract()?;
        let lineno: u32 = tuple.get_item(2)?.extract()?;
        let func_name = nodeid.rsplit("::").next().unwrap_or(&nodeid).to_string();
        items.push(TestItem {
            nodeid,
            path: path.to_path_buf(),
            module_name: "__doctest_textfile__".to_string(),
            func_name,
            func,
            cls: None,
            is_coroutine: false,
            is_doctest: true,
            fixture_names: vec!["doctest_namespace".to_string(), "request".to_string()],
            extra_fixture_names: vec![],
            marks: vec![],
            callspec: vec![],
            fixture_params: vec![],
            lineno,
            collector_class: String::new(),
            func_class: String::new(),
            py_node: None,
            max_param_scope: crate::fixture::Scope::Function,
            scope_sort_keys: Vec::new(),
        });
    }
    Ok(())
}

/// Check whether a file path matches --doctest-glob patterns.
pub fn is_doctest_textfile(py: Python<'_>, path: &Path, py_config: &Py<PyAny>) -> PyResult<bool> {
    let doctest_mod = py.import("_pytest.doctest")?;
    let result = doctest_mod
        .getattr("is_doctest_textfile")?
        .call1((path.to_string_lossy().as_ref(), py_config.bind(py)))?;
    result.extract()
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn introspect_namespace(
    py: Python<'_>,
    module: &Bound<'_, PyModule>,
    nodeid_base: &str,
    module_name: &str,
    path: &Path,
    items: &mut Vec<TestItem>,
    registry: &mut FixtureRegistry,
    extra_generate_hooks: &[Py<PyAny>],
    makeitem_hook: bool,
    filters: &NameFilters,
) -> PyResult<()> {
    register_fixtures_from(py, module, &format!("{nodeid_base}::"), registry)?;

    // Module __test__ = False: skip test collection entirely (nose compat).
    if module
        .getattr("__test__")
        .ok()
        .is_some_and(|a| !a.extract::<bool>().unwrap_or(true))
    {
        return Ok(());
    }

    // Module-level `pytestmark` applies to every item in the module.
    let module_marks = read_marks(py, module.as_any())?;

    // pytest_generate_tests impls parametrize via metafunc: the module-level
    // one plus every plugin/conftest impl (pytest-repeat registers one). They
    // run in order on a single combined callable.
    let gen_list = pyo3::types::PyList::empty(py);
    if let Some(mod_hook) = module
        .dict()
        .get_item("pytest_generate_tests")?
        .filter(|hook| hook.is_callable())
    {
        gen_list.append(mod_hook)?;
    }
    for hook in extra_generate_hooks {
        gen_list.append(hook.bind(py))?;
    }
    let generate_hook: Option<Bound<'_, PyAny>> = if gen_list.is_empty() {
        None
    } else {
        Some(
            py.import("pytest._metafunc")?
                .getattr("combine_generate_hooks")?
                .call1((gen_list,))?,
        )
    };

    let inspect = py.import("inspect")?;
    let isclass = inspect.getattr("isclass")?;
    let dict = module.dict();
    // Module dicts preserve definition order in CPython; keep it.
    for (key, value) in dict.iter() {
        let Ok(name) = key.extract::<String>() else {
            continue;
        };
        // Builtin attributes (dunders etc.) are ignored before any matching, so
        // python_functions=* / python_classes=* don't collect or warn on them.
        if filters.is_ignored(&name) {
            continue;
        }
        // pytest_pycollect_makeitem: a conftest may claim a namespace member
        // (even a non-`test`-named one) by returning a custom node, e.g.
        // `MyFunction.from_parent(name=name, parent=collector)`. Honor it so the
        // tree renders `<MyFunction some>`; otherwise fall through to the
        // default Rust collection path.
        if makeitem_hook
            && let Some(custom) = fire_pycollect_makeitem(
                py,
                nodeid_base,
                path,
                &name,
                &value,
                filters.matches_function(&name),
            )
        {
            for (class_name, node_name, py_node) in custom {
                // Resolve fixture names from the original callobj so the
                // runner can fill fixtures even when a custom node class was
                // returned by the hook (the custom node may have an empty
                // fixturenames list at this point).
                let (fixture_names, _) =
                    param_names_with_positional_only(py, &value).unwrap_or_default();
                items.push(TestItem {
                    nodeid: format!("{nodeid_base}::{node_name}"),
                    path: path.to_path_buf(),
                    module_name: module_name.to_string(),
                    func_name: node_name,
                    func: value.clone().unbind(),
                    cls: None,
                    is_coroutine: false,
                    is_doctest: false,
                    fixture_names,
                    extra_fixture_names: Vec::new(),
                    marks: Vec::new(),
                    callspec: Vec::new(),
                    fixture_params: Vec::new(),
                    lineno: 0,
                    collector_class: String::new(),
                    func_class: class_name,
                    py_node,
                    max_param_scope: crate::fixture::Scope::Function,
                    scope_sort_keys: Vec::new(),
                });
            }
            continue;
        }
        // Wrap isclass in try-catch: objects with __class__ = property(raises)
        // cause inspect.isclass → isinstance(obj, type) to raise (#4266).
        let is_class = isclass
            .call1((&value,))
            .and_then(|r| r.extract::<bool>())
            .unwrap_or(false);
        if is_class {
            let is_testcase: bool = py
                .import("pytest._unittest")?
                .getattr("is_testcase_class")?
                .call1((&value,))?
                .extract()?;
            if is_testcase {
                // Class __test__ = False: skip (nose compat).
                let test_attr = value.getattr("__test__").ok();
                let test_false = test_attr
                    .as_ref()
                    .and_then(|a| a.extract::<bool>().ok())
                    .is_some_and(|v| !v);
                // Abstract TestCase classes are not collected (#12275).
                let is_abstract: bool =
                    inspect.getattr("isabstract")?.call1((&value,))?.extract()?;
                if !is_abstract && !test_false {
                    collect_testcase(
                        py,
                        &value,
                        &name,
                        nodeid_base,
                        module_name,
                        path,
                        &module_marks,
                        items,
                        registry,
                    )?;
                }
            } else if filters.matches_class(&name) {
                // Class __test__ = False: skip (nose compat).
                let test_attr = value.getattr("__test__").ok();
                let test_false = test_attr
                    .as_ref()
                    .and_then(|a| a.extract::<bool>().ok())
                    .is_some_and(|v| !v);
                let is_abstract: bool =
                    inspect.getattr("isabstract")?.call1((&value,))?.extract()?;
                if !is_abstract && !test_false {
                    collect_class(
                        py,
                        &value,
                        &name,
                        nodeid_base,
                        module_name,
                        path,
                        &module_marks,
                        items,
                        registry,
                        module,
                        generate_hook.as_ref(),
                        filters,
                    )?;
                }
            }
            continue;
        }
        // Test functions match the python_functions ini patterns (default
        // prefix "test"); fixtures are never test functions.
        if !filters.matches_function(&name)
            || !value.is_callable()
            || value.hasattr("_pytestfixturefunction").unwrap_or(false)
        {
            continue;
        }
        // Function __test__ = False: skip (nose compat).
        if value
            .getattr("__test__")
            .ok()
            .is_some_and(|a| !a.extract::<bool>().unwrap_or(true))
        {
            continue;
        }
        // A test-named member that is callable but not a function (an instance
        // with __call__) cannot be collected: pytest warns and skips it.
        let skip_nonfunc: bool = py
            .import("pytest._pycollect")?
            .getattr("warn_uncollectable_function")?
            .call1((&name, &value, path.to_string_lossy().as_ref()))?
            .extract()?;
        if skip_nonfunc {
            continue;
        }
        // Generator test functions fail collection (#12960).
        if inspect
            .getattr("isgeneratorfunction")?
            .call1((&value,))?
            .extract::<bool>()
            .unwrap_or(false)
        {
            return Err(collect_error(
                py,
                &format!("'yield' keyword is allowed in fixtures, but not in tests ({name})"),
            ));
        }
        let mut marks = read_marks(py, &value)?;
        marks.extend(clone_marks(py, &module_marks));
        push_test_items(
            py,
            items,
            nodeid_base,
            module_name,
            path,
            &name,
            &value,
            None,
            false,
            marks,
            module,
            generate_hook.as_ref(),
            registry,
        )?;
    }
    Ok(())
}

pub(crate) fn clone_marks(py: Python<'_>, marks: &[MarkData]) -> Vec<MarkData> {
    marks
        .iter()
        .map(|m| MarkData {
            name: m.name.clone(),
            obj: m.obj.clone_ref(py),
        })
        .collect()
}

/// Collect unittest.TestCase test methods as zero-arg runner callables
/// (setUp/method/tearDown handled by the pytest._unittest shim).
#[allow(clippy::too_many_arguments)]
pub(crate) fn collect_testcase(
    py: Python<'_>,
    cls: &Bound<'_, PyAny>,
    cls_name: &str,
    nodeid_base: &str,
    module_name: &str,
    path: &Path,
    module_marks: &[MarkData],
    items: &mut Vec<TestItem>,
    registry: &mut FixtureRegistry,
) -> PyResult<()> {
    let unittest_shim = py.import("pytest._unittest")?;
    let make_runner = unittest_shim.getattr("make_runner")?;
    let class_nodeid = format!("{nodeid_base}::{cls_name}");
    let mut class_marks = read_marks(py, cls)?;
    class_marks.extend(clone_marks(py, module_marks));

    // @pytest.fixture methods on the TestCase (upstream supports them,
    // typically autouse=True stashing values on self); they bind to the
    // same instance the test runs on via the runner's make_case().
    for pair in cls.getattr("__dict__")?.call_method0("items")?.try_iter()? {
        let (name, value): (String, Bound<'_, PyAny>) = pair?.extract()?;
        if value.is_callable() && value.hasattr("_pytestfixturefunction")? {
            register_fixture_def(
                py,
                &name,
                &value,
                &format!("{class_nodeid}::"),
                true,
                registry,
            )?;
        }
    }

    // Upstream's injected autouse fixtures: setUpClass/tearDownClass
    // (+doClassCleanups), pytest-style setup_class/teardown_class and
    // setup_method/teardown_method. Skipped classes don't register them
    // (upstream gates on _is_skipped(cls)).
    let class_skipped: bool = cls
        .getattr("__unittest_skip__")
        .and_then(|v| v.extract())
        .unwrap_or(false);
    if !class_skipped {
        for (factory, needs_instance) in [
            ("make_setup_method_fixture", true),
            ("make_class_fixture", false),
            ("make_setup_class_fixture", false),
        ] {
            let fixture = unittest_shim.getattr(factory)?.call1((cls,))?;
            if !fixture.is_none() {
                register_fixture_def(
                    py,
                    "",
                    &fixture,
                    &format!("{class_nodeid}::"),
                    needs_instance,
                    registry,
                )?;
            }
        }
    }

    // dir() includes inherited test methods, matching unittest collection.
    let mut names: Vec<String> = py
        .import("builtins")?
        .getattr("dir")?
        .call1((cls,))?
        .extract()?;
    names.sort();
    names.retain(|name| {
        name.starts_with("test")
            && cls
                .getattr(name.as_str())
                .map(|method| {
                    // Methods opting out via __test__ = False (issue1558).
                    method.is_callable()
                        && method
                            .getattr("__test__")
                            .and_then(|v| v.extract::<bool>())
                            .unwrap_or(true)
                })
                .unwrap_or(false)
    });
    // No test methods: unittest's runTest fallback collects as a single
    // item (upstream skips twisted.trial's own runTest; twisted-less here).
    if names.is_empty()
        && cls
            .getattr("runTest")
            .map(|method| method.is_callable())
            .unwrap_or(false)
    {
        names.push("runTest".to_string());
    }
    for name in names {
        let Ok(method) = cls.getattr(name.as_str()) else {
            continue;
        };
        if !method.is_callable() {
            continue;
        }
        let mut marks = read_marks(py, &method)?;
        marks.extend(clone_marks(py, &class_marks));
        let runner = make_runner.call1((cls, name.as_str()))?;
        items.push(TestItem {
            nodeid: format!("{class_nodeid}::{name}"),
            path: path.to_path_buf(),
            module_name: module_name.to_string(),
            func_name: name,
            func: runner.unbind(),
            // cls stays None: the runner drives the unittest instance via
            // make_case (item.cls Some would make the engine instantiate and
            // rebind, bypassing setUp/tearDown). The class is still exposed
            // for node.cls introspection via the runner's `cls` attribute.
            cls: None,
            is_coroutine: false,
            is_doctest: false,
            fixture_names: Vec::new(),
            extra_fixture_names: Vec::new(),
            marks,
            callspec: Vec::new(),
            fixture_params: Vec::new(),
            lineno: first_lineno(py, &method),
            collector_class: String::new(),
            func_class: String::new(),
            py_node: None,
            max_param_scope: crate::fixture::Scope::Function,
            scope_sort_keys: Vec::new(),
        });
    }
    Ok(())
}

/// Collect test methods (and class-level fixtures) from a Test* class.
#[allow(clippy::too_many_arguments)]
pub(crate) fn collect_class(
    py: Python<'_>,
    cls: &Bound<'_, PyAny>,
    cls_name: &str,
    nodeid_base: &str,
    module_name: &str,
    path: &Path,
    module_marks: &[MarkData],
    items: &mut Vec<TestItem>,
    registry: &mut FixtureRegistry,
    module: &Bound<'_, PyModule>,
    generate_hook: Option<&Bound<'_, PyAny>>,
    filters: &NameFilters,
) -> PyResult<()> {
    // Test classes with a custom __init__/__new__ can't be instantiated for
    // collection: pytest warns and skips them (handled in the Python shim so
    // the PytestCollectionWarning is captured for the warnings summary).
    let skip_class: bool = py
        .import("pytest._pycollect")?
        .getattr("warn_uncollectable_class")?
        .call1((cls, nodeid_base))?
        .extract()?;
    if skip_class {
        return Ok(());
    }
    let class_nodeid = format!("{nodeid_base}::{cls_name}");
    let mut class_marks = read_marks(py, cls)?;
    class_marks.extend(clone_marks(py, module_marks));

    let builtins = py.import("builtins")?;
    let staticmethod_type = builtins.getattr("staticmethod")?;
    let classmethod_type = builtins.getattr("classmethod")?;

    // Definition order, matching pytest's PyCollector.collect: walk the MRO
    // (most-derived first), gather each class's own __dict__ in definition
    // order (deduped by name across the MRO), then concatenate in reverse-MRO
    // order so inherited methods precede the subclass's own. This is stable
    // even when a method is aliased from a base (e.g. `test_bar = Base.test_bar`),
    // where the function's own lineno would otherwise flip the order.
    let mro: Vec<Bound<'_, PyAny>> = cls.getattr("__mro__")?.extract()?;
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut per_class: Vec<Vec<(String, Bound<'_, PyAny>, bool)>> = Vec::new();
    for base in &mro {
        let base_dict = base.getattr("__dict__")?;
        let mut values: Vec<(String, Bound<'_, PyAny>, bool)> = Vec::new();
        for pair in base_dict.call_method0("items")?.try_iter()? {
            let (key, raw): (Bound<'_, PyAny>, Bound<'_, PyAny>) = pair?.extract()?;
            let Ok(name) = key.extract::<String>() else {
                continue;
            };
            // Builtin attributes (dunders) are ignored before matching, so
            // python_functions=* doesn't collect every inherited method.
            if filters.is_ignored(&name) || seen.contains(&name) {
                continue;
            }
            seen.insert(name.clone());

            let is_static = raw.is_instance(&staticmethod_type)?;
            let is_classmethod = raw.is_instance(&classmethod_type)?;
            let value = if is_static || is_classmethod {
                raw.getattr("__func__")?
            } else {
                raw
            };
            if !value.is_callable() {
                continue;
            }
            values.push((name, value, is_static));
        }
        per_class.push(values);
    }

    for (name, value, is_static) in per_class.into_iter().rev().flatten() {
        if value.hasattr("_pytestfixturefunction")? {
            register_fixture_def(
                py,
                &name,
                &value,
                &format!("{class_nodeid}::"),
                !is_static,
                registry,
            )?;
            continue;
        }
        if !filters.matches_function(&name) {
            continue;
        }
        // Method __test__ = False: skip (nose compat).
        if value
            .getattr("__test__")
            .ok()
            .is_some_and(|a| !a.extract::<bool>().unwrap_or(true))
        {
            continue;
        }
        let mut marks = read_marks(py, &value)?;
        for mark in &class_marks {
            marks.push(MarkData {
                name: mark.name.clone(),
                obj: mark.obj.clone_ref(py),
            });
        }
        push_test_items(
            py,
            items,
            &class_nodeid,
            module_name,
            path,
            &name,
            &value,
            Some(cls),
            is_static,
            marks,
            module,
            generate_hook,
            registry,
        )?;
    }
    // Fire pytest_pycollect_makeitem hooks so plugins can set extra_keyword_matches.
    // Store the result on the class object for keyword_match_names to read.
    let _ = (|| -> PyResult<()> {
        let kw_set = py
            .import("pytest._node")?
            .call_method1("fire_makeitem_for_class", (cls_name,))?;
        let mut extra: Vec<String> = Vec::new();
        for item in kw_set.try_iter()? {
            if let Ok(s) = item?.extract::<String>() {
                extra.push(s);
            }
        }
        if !extra.is_empty() {
            cls.setattr("_pytest_extra_keyword_matches", extra)?;
        }
        Ok(())
    })();
    Ok(())
}

/// Push the (possibly parametrize-expanded) items for one test function.
#[allow(clippy::too_many_arguments)]
pub(crate) fn push_test_items(
    py: Python<'_>,
    items: &mut Vec<TestItem>,
    nodeid_prefix: &str,
    module_name: &str,
    path: &Path,
    name: &str,
    func: &Bound<'_, PyAny>,
    cls: Option<&Bound<'_, PyAny>>,
    is_static: bool,
    marks: Vec<MarkData>,
    module: &Bound<'_, PyModule>,
    generate_hook: Option<&Bound<'_, PyAny>>,
    registry: &FixtureRegistry,
) -> PyResult<()> {
    let flags = async_flags(py, func)?;
    let (mut fixture_names, has_positional_only) = param_names_with_positional_only(py, func)?;
    // For non-static class methods, strip the first parameter (self/cls)
    // regardless of its name — pytest does the same in getfuncargnames.
    // When any positional-only parameter exists, the self/cls was already
    // excluded by param_names (it only collects POSITIONAL_OR_KEYWORD and
    // KEYWORD_ONLY), so skip the strip — matching pytest's getfuncargnames.
    if cls.is_some() && !is_static && !has_positional_only && !fixture_names.is_empty() {
        fixture_names.remove(0);
    }
    // @unittest.mock.patch-injected leading params are not fixture requests.
    let mock_args = num_mock_patch_args(py, func).min(fixture_names.len());
    if mock_args > 0 {
        fixture_names.drain(..mock_args);
    }

    // pytest_generate_tests: metafunc.parametrize calls become parametrize
    // marks, merged after the decorator-applied ones.
    let mut marks = marks;
    let fixture_names = fixture_names;
    // Transitive fixture closure: walk fixture deps so parametrize argnames
    // that reference indirect dependencies (e.g. fix2 when test_it requests
    // fix1 which depends on fix2) are recognized by validation.
    let test_nodeid = format!("{nodeid_prefix}::{name}");
    let mut closure_names: Vec<String> = fixture_names.clone();
    {
        let mut seen: std::collections::HashSet<String> = closure_names.iter().cloned().collect();
        let mut i = 0;
        while i < closure_names.len() {
            if let Some(def) = registry.lookup(&closure_names[i], &test_nodeid) {
                for dep in &def.param_names {
                    if dep != "request" && seen.insert(dep.clone()) {
                        closure_names.push(dep.clone());
                    }
                }
            }
            i += 1;
        }
    }
    // Fixtures a generate hook appended (pytest-repeat's indirect step
    // fixture): set up so their request.param is consumed, but not injected
    // into the test signature.
    let mut extra_generated_fixtures: Vec<String> = Vec::new();
    if let Some(hook) = generate_hook {
        // metafunc.config (option.count etc.) and definition markers
        // (get_closest_marker) let plugin impls like pytest-repeat decide.
        let config = crate::python::proxies::existing_py_config(py).map(|c| c.into_bound(py));
        let mark_objs = pyo3::types::PyList::empty(py);
        for m in &marks {
            mark_objs.append(m.obj.bind(py))?;
        }
        let metafunc = py.import("pytest._metafunc")?.getattr("Metafunc")?.call1((
            func,
            closure_names.clone(),
            module,
            cls.map(|c| c.clone().unbind()),
            config,
            mark_objs,
        ))?;
        hook.call1((&metafunc,))?;
        for mark in metafunc.getattr("_parametrize_marks")?.try_iter()? {
            let mark = mark?;
            marks.push(MarkData {
                name: "parametrize".to_string(),
                obj: mark.unbind(),
            });
        }
        // A hook may append fixturenames (pytest-repeat adds its indirect
        // step fixture so its request.param is set up per repeat).
        let updated: Vec<String> = metafunc.getattr("fixturenames")?.extract()?;
        for name in updated {
            if !fixture_names.contains(&name) && !extra_generated_fixtures.contains(&name) {
                extra_generated_fixtures.push(name);
            }
        }
    }

    validate_parametrize_argnames(
        py,
        &marks,
        name,
        &closure_names,
        &extra_generated_fixtures,
        func,
        registry,
        &test_nodeid,
    )?;

    let variants = expand_parametrize(py, &marks, &test_nodeid, Some(func))?;
    for variant in variants {
        let nodeid = match &variant.id {
            Some(id) => format!("{nodeid_prefix}::{name}[{id}]"),
            None => format!("{nodeid_prefix}::{name}"),
        };
        let mut item_marks: Vec<MarkData> = marks
            .iter()
            .map(|m| MarkData {
                name: m.name.clone(),
                obj: m.obj.clone_ref(py),
            })
            .collect();
        item_marks.extend(variant.extra_marks);
        items.push(TestItem {
            nodeid,
            path: path.to_path_buf(),
            module_name: module_name.to_string(),
            func_name: name.to_string(),
            func: func.clone().unbind(),
            cls: cls.map(|c| c.clone().unbind()),
            is_coroutine: flags.is_coroutine,
            is_doctest: false,
            fixture_names: fixture_names.clone(),
            extra_fixture_names: extra_generated_fixtures.clone(),
            marks: item_marks,
            callspec: variant.params,
            fixture_params: variant.indirect_params,
            lineno: first_lineno(py, func),
            collector_class: String::new(),
            func_class: String::new(),
            py_node: None,
            max_param_scope: variant.max_param_scope,
            scope_sort_keys: variant.scope_sort_keys,
        });
    }
    Ok(())
}

pub(crate) struct ParamVariant {
    /// The "[...]" id suffix; None for unparametrized tests.
    id: Option<String>,
    params: Vec<(String, Py<PyAny>)>,
    /// indirect parametrize assignments: (fixture name, param index, value).
    indirect_params: Vec<(String, usize, Py<PyAny>)>,
    /// Marks attached via pytest.param(..., marks=...).
    extra_marks: Vec<MarkData>,
    /// Highest parametrize scope across all dimensions (for item reordering).
    max_param_scope: crate::fixture::Scope,
    /// Per non-function-scoped dimension: (argname, scope, 0-based set index).
    /// Feeds `reorder_items` so items sharing a high-scope param value group.
    scope_sort_keys: Vec<(String, crate::fixture::Scope, usize)>,
}

/// One parameter set (one `pytest.param`/value row) within a single
/// `@pytest.mark.parametrize` mark.
struct ParamSet {
    /// None hides the set from the test ID (pytest.HIDDEN_PARAM).
    id_part: Option<String>,
    params: Vec<(String, Py<PyAny>)>,
    /// indirect=True/[names]: the value parametrizes the same-named
    /// fixture (request.param) instead of being passed to the test.
    indirect_params: Vec<(String, usize, Py<PyAny>)>,
    extra_marks: Vec<MarkData>,
}

/// One `@pytest.mark.parametrize` mark's worth of parameter sets; stacked
/// marks become separate dimensions in the cartesian product.
struct Dim {
    sets: Vec<ParamSet>,
    scope: crate::fixture::Scope,
}

/// Expand stacked @pytest.mark.parametrize marks into the cartesian product
/// Validate that parametrize argnames are either function parameters or
/// known fixtures. Raises Failed(pytrace=False) like upstream's
/// `Metafunc._validate_if_using_arg_names`.
#[allow(clippy::too_many_arguments)]
fn validate_parametrize_argnames(
    py: Python<'_>,
    marks: &[MarkData],
    func_name: &str,
    fixture_names: &[String],
    extra_fixture_names: &[String],
    func: &Bound<'_, PyAny>,
    registry: &FixtureRegistry,
    test_nodeid: &str,
) -> PyResult<()> {
    let inspect = py.import("inspect")?;
    let all_params: std::collections::HashSet<String> = inspect
        .call_method1("signature", (func,))
        .and_then(|sig| sig.getattr("parameters"))
        .and_then(|params| {
            params
                .call_method0("keys")?
                .try_iter()?
                .map(|k| k.and_then(|v| v.extract::<String>()))
                .collect()
        })
        .unwrap_or_default();

    for mark in marks.iter().filter(|m| m.name == "parametrize") {
        let args = mark.obj.bind(py).getattr("args")?;
        if args.len()? == 0 {
            continue;
        }
        let argnames_obj = args.get_item(0)?;
        let argnames: Vec<String> = match argnames_obj.extract::<String>() {
            Ok(joined) => joined
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect(),
            Err(_) => argnames_obj.extract()?,
        };
        let indirect_obj = mark
            .obj
            .bind(py)
            .getattr("kwargs")?
            .get_item("indirect")
            .ok();
        let indirect_all = indirect_obj
            .as_ref()
            .and_then(|v| v.extract::<bool>().ok())
            .unwrap_or(false);
        let indirect_names: Vec<String> = indirect_obj
            .as_ref()
            .and_then(|v| {
                v.extract::<Vec<String>>()
                    .ok()
                    .or_else(|| v.extract::<String>().ok().map(|s| vec![s]))
            })
            .unwrap_or_default();

        for argname in &argnames {
            if argname == "request"
                || fixture_names.contains(argname)
                || extra_fixture_names.contains(argname)
                || all_params.contains(argname.as_str())
                || registry.lookup(argname, test_nodeid).is_some()
            {
                continue;
            }
            let kind = if indirect_all || indirect_names.iter().any(|n| n == argname) {
                "fixture"
            } else {
                "argument"
            };
            let msg = format!("In {func_name}: function uses no {kind} '{argname}'");
            let failed_result: PyResult<PyErr> = (|| {
                let cls = py.import("_pytest.outcomes")?.getattr("Failed")?;
                let instance = cls.call1((&msg,))?;
                instance.setattr("pytrace", false)?;
                Ok(PyErr::from_value(instance))
            })();
            return Err(match failed_result {
                Ok(err) => err,
                Err(_) => collect_error(py, &msg),
            });
        }
    }
    Ok(())
}

/// of parameter sets. Marks appear in pytestmark order (bottom decorator
/// first); ids join in that order and later marks vary fastest.
pub(crate) fn expand_parametrize(
    py: Python<'_>,
    marks: &[MarkData],
    nodeid: &str,
    func: Option<&Bound<'_, PyAny>>,
) -> PyResult<Vec<ParamVariant>> {
    let param_spec_cls = py.import("pytest")?.getattr("ParamSpec")?;
    let hidden_param = py.import("pytest")?.getattr("HIDDEN_PARAM")?;
    // Armed by configure_mark_generator once the session config is known.
    let strict_ids = py
        .import("pytest._marks")
        .and_then(|m| m.getattr("mark"))
        .and_then(|mark| mark.getattr("_strict_parametrization_ids"))
        .and_then(|v| v.extract::<bool>())
        .unwrap_or(false);
    let mut dims: Vec<Dim> = Vec::new();

    for mark in marks.iter().filter(|m| m.name == "parametrize") {
        let args = mark.obj.bind(py).getattr("args")?;
        let kwargs = mark.obj.bind(py).getattr("kwargs")?;
        // Both spellings are valid: positional or argnames=/argvalues= keywords.
        let argnames_obj = if args.len()? > 0 {
            args.get_item(0)?
        } else {
            kwargs.get_item("argnames")?
        };
        let argvalues = if args.len()? > 1 {
            args.get_item(1)?
        } else {
            kwargs.get_item("argvalues")?
        };
        // pytest's force_tuple: only a single argname given as a *string*
        // takes each argvalue as the bare value; a one-element list
        // (["x"]) still expects one-element value collections.
        let (argnames, force_scalar): (Vec<String>, bool) = match argnames_obj.extract::<String>() {
            Ok(joined) => {
                // Strip trailing empty names so that a trailing comma in the
                // argnames string (e.g. "a,b,c,") is silently ignored, matching
                // real pytest's behaviour.
                let mut names: Vec<String> =
                    joined.split(',').map(|s| s.trim().to_string()).collect();
                while names.last().map(|s: &String| s.is_empty()).unwrap_or(false) {
                    names.pop();
                }
                let single = names.len() == 1;
                (names, single)
            }
            Err(_) => (argnames_obj.extract()?, false),
        };
        let ids_obj = mark.obj.bind(py).getattr("kwargs")?.get_item("ids").ok();
        let n_argvalues = argvalues.len().unwrap_or(usize::MAX);
        let explicit_ids: Option<Vec<(Option<String>, bool)>> = ids_obj.as_ref().and_then(|ids| {
            if ids.is_callable() {
                return None;
            }
            let iter = ids.try_iter().ok()?;
            let mut result = Vec::new();
            for id in iter {
                if result.len() >= n_argvalues {
                    break;
                }
                let id = id.ok()?;
                if id.is(&hidden_param) {
                    result.push((None, true));
                } else if id.is_none() {
                    result.push((None, false));
                } else {
                    result.push((Some(id.extract::<String>().ok()?), false));
                }
            }
            Some(result)
        });
        // ids=callable: idfn(val) per value, None falling through to the
        // default id for that value (upstream _idval_from_function).
        let ids_callable = ids_obj.filter(|ids| ids.is_callable());
        // indirect=True routes every argname's value to the same-named
        // fixture's request.param; indirect=["x"] only the listed ones.
        let indirect_obj = mark
            .obj
            .bind(py)
            .getattr("kwargs")?
            .get_item("indirect")
            .ok();
        let indirect_all = indirect_obj
            .as_ref()
            .and_then(|value| value.extract::<bool>().ok())
            .unwrap_or(false);
        let indirect_names: Vec<String> = indirect_obj
            .as_ref()
            .and_then(|value| {
                value
                    .extract::<Vec<String>>()
                    .ok()
                    .or_else(|| value.extract::<String>().ok().map(|s| vec![s]))
            })
            .unwrap_or_default();
        let is_indirect = |name: &str| indirect_all || indirect_names.iter().any(|n| n == name);
        let dim_scope = mark
            .obj
            .bind(py)
            .getattr("kwargs")?
            .get_item("scope")
            .ok()
            .and_then(|s| s.extract::<String>().ok())
            .and_then(|s| crate::fixture::Scope::parse(&s))
            .unwrap_or(crate::fixture::Scope::Function);

        let mut sets = Vec::new();
        for (index, value_set) in argvalues.try_iter()?.enumerate() {
            let value_set = value_set?;
            let (values, spec_id, mut hidden, extra_marks) =
                if value_set.is_instance(&param_spec_cls)? {
                    let values: Vec<Bound<'_, PyAny>> = value_set
                        .getattr("values")?
                        .try_iter()?
                        .collect::<PyResult<_>>()?;
                    let id_obj = value_set.getattr("id")?;
                    let (spec_id, hidden) = if id_obj.is(&hidden_param) {
                        (None, true)
                    } else {
                        (id_obj.extract::<Option<String>>()?, false)
                    };
                    let extra_marks = value_set
                        .getattr("marks")?
                        .try_iter()?
                        .map(|m| {
                            let m = m?;
                            Ok(MarkData {
                                name: m.getattr("name")?.extract()?,
                                obj: m.unbind(),
                            })
                        })
                        .collect::<PyResult<Vec<_>>>()?;
                    (values, spec_id, hidden, extra_marks)
                } else if force_scalar {
                    (vec![value_set], None, false, Vec::new())
                } else {
                    let values: Vec<Bound<'_, PyAny>> =
                        value_set.try_iter()?.collect::<PyResult<_>>()?;
                    (values, None, false, Vec::new())
                };

            if values.len() != argnames.len() {
                // Upstream ParameterSet._for_parametrize wording, raised as
                // a bare CollectError (message only, no traceback).
                let names_repr = format!(
                    "[{}]",
                    argnames
                        .iter()
                        .map(|n| format!("'{n}'"))
                        .collect::<Vec<_>>()
                        .join(", ")
                );
                let values_repr: String = pyo3::types::PyTuple::new(py, values.iter())?
                    .repr()?
                    .extract()?;
                let message = format!(
                    "{nodeid}: in \"parametrize\" the number of names ({}):\n  {names_repr}\nmust be equal to the number of values ({}):\n  {values_repr}",
                    argnames.len(),
                    values.len()
                );
                return Err(collect_error(py, &message));
            }

            // Check explicit_ids for HIDDEN_PARAM at this index.
            if !hidden
                && let Some(ref ids) = explicit_ids
                && let Some((_, is_hidden)) = ids.get(index)
                && *is_hidden
            {
                hidden = true;
            }

            let id_part = if hidden {
                None
            } else {
                let from_callable = || -> PyResult<Option<String>> {
                    let Some(idfn) = ids_callable.as_ref() else {
                        return Ok(None);
                    };
                    let mut parts = Vec::new();
                    for (argname, value) in argnames.iter().zip(values.iter()) {
                        let id = idfn.call1((value,)).map_err(|err| {
                            collect_error(
                                py,
                                &format!(
                                    "{nodeid}: error raised while trying to determine id of \
                                     parameter '{argname}' at position {index}\n{}",
                                    err.value(py)
                                ),
                            )
                        })?;
                        parts.push(
                            user_id_from_value(py, &id)
                                .unwrap_or_else(|| id_for_value(value, argname, index)),
                        );
                    }
                    Ok(Some(parts.join("-")))
                };
                let callable_id = from_callable()?;
                Some(
                    spec_id
                        .or_else(|| {
                            explicit_ids
                                .as_ref()
                                .and_then(|ids| ids.get(index).and_then(|(s, _)| s.clone()))
                        })
                        .or(callable_id)
                        .unwrap_or_else(|| {
                            let parts: Vec<String> = argnames
                                .iter()
                                .zip(values.iter())
                                .map(|(argname, value)| id_for_value(value, argname, index))
                                .collect();
                            parts.join("-")
                        }),
                )
            };
            let mut params: Vec<(String, Py<PyAny>)> = Vec::new();
            let mut indirect_params: Vec<(String, usize, Py<PyAny>)> = Vec::new();
            for (argname, value) in argnames.iter().cloned().zip(values) {
                if is_indirect(&argname) {
                    indirect_params.push((argname, index, value.unbind()));
                } else {
                    params.push((argname, value.unbind()));
                }
            }
            sets.push(ParamSet {
                id_part,
                params,
                indirect_params,
                extra_marks,
            });
        }
        dedup_param_ids(py, &mut sets, nodeid, &argnames, strict_ids)?;
        if sets.is_empty() {
            sets.push(notset_param_set(
                py,
                &argnames,
                func,
                indirect_all,
                &indirect_names,
            )?);
        }
        dims.push(Dim {
            sets,
            scope: dim_scope,
        });
    }

    if dims.is_empty() {
        return Ok(vec![ParamVariant {
            id: None,
            params: Vec::new(),
            indirect_params: Vec::new(),
            extra_marks: Vec::new(),
            max_param_scope: crate::fixture::Scope::Function,
            scope_sort_keys: Vec::new(),
        }]);
    }
    // An empty parameter set produces no items (pytest marks one skipped;
    // zero items is the closest simple behavior).
    if dims.iter().any(|dim| dim.sets.is_empty()) {
        return Ok(Vec::new());
    }

    Ok(cartesian_param_variants(py, &dims))
}

/// Cartesian product over the parametrize dimensions: the last dim varies
/// fastest and IDs join in dim order (matching stacked-decorator order).
fn cartesian_param_variants(py: Python<'_>, dims: &[Dim]) -> Vec<ParamVariant> {
    let mut variants = Vec::new();
    let mut indices = vec![0usize; dims.len()];
    'outer: loop {
        let mut id_parts = Vec::new();
        let mut params = Vec::new();
        let mut indirect_params = Vec::new();
        let mut extra_marks = Vec::new();
        for (dim, &index) in dims.iter().zip(indices.iter()) {
            let set = &dim.sets[index];
            // HIDDEN_PARAM sets contribute nothing to the test ID.
            if let Some(part) = &set.id_part {
                id_parts.push(part.clone());
            }
            for (name, value) in &set.params {
                params.push((name.clone(), value.clone_ref(py)));
            }
            for (name, param_index, value) in &set.indirect_params {
                indirect_params.push((name.clone(), *param_index, value.clone_ref(py)));
            }
            for mark in &set.extra_marks {
                extra_marks.push(MarkData {
                    name: mark.name.clone(),
                    obj: mark.obj.clone_ref(py),
                });
            }
        }
        let max_param_scope = dims
            .iter()
            .map(|d| d.scope)
            .max()
            .unwrap_or(crate::fixture::Scope::Function);
        let scope_sort_keys: Vec<(String, crate::fixture::Scope, usize)> = dims
            .iter()
            .zip(indices.iter())
            .filter(|(d, _)| d.scope > crate::fixture::Scope::Function)
            .map(|(d, &idx)| {
                let set = &d.sets[idx];
                let mut names: Vec<&str> = set.params.iter().map(|(n, _)| n.as_str()).collect();
                names.extend(set.indirect_params.iter().map(|(n, _, _)| n.as_str()));
                (names.join(","), d.scope, idx)
            })
            .collect();
        variants.push(ParamVariant {
            // All-hidden variants keep the bare test name (no brackets).
            id: (!id_parts.is_empty()).then(|| id_parts.join("-")),
            params,
            indirect_params,
            extra_marks,
            max_param_scope,
            scope_sort_keys,
        });

        for pos in (0..dims.len()).rev() {
            indices[pos] += 1;
            if indices[pos] < dims[pos].sets.len() {
                continue 'outer;
            }
            indices[pos] = 0;
            if pos == 0 {
                break 'outer;
            }
        }
    }
    variants
}

/// Resolve duplicate parameter-set IDs within one parametrize mark:
/// under strict_parametrization_ids a duplicate is a CollectError;
/// otherwise pytest's make_unique_parameterset_ids counter suffix applies.
fn dedup_param_ids(
    py: Python<'_>,
    sets: &mut [ParamSet],
    nodeid: &str,
    argnames: &[String],
    strict_ids: bool,
) -> PyResult<()> {
    let mut counts: std::collections::HashMap<Option<String>, usize> =
        std::collections::HashMap::new();
    for set in sets.iter() {
        *counts.entry(set.id_part.clone()).or_default() += 1;
    }
    if counts.values().any(|&count| count > 1) {
        let display = |id: &Option<String>| id.clone().unwrap_or_else(|| "<hidden>".to_string());
        if strict_ids {
            let mut reprs = Vec::new();
            for set in sets.iter() {
                let values = PyList::new(py, set.params.iter().map(|(_, value)| value.bind(py)))?;
                reprs.push(values.repr()?.to_string());
            }
            let mut seen = std::collections::HashSet::new();
            let duplicates: Vec<String> = sets
                .iter()
                .filter(|set| counts[&set.id_part] > 1)
                .filter(|set| seen.insert(set.id_part.clone()))
                .map(|set| display(&set.id_part))
                .collect();
            let ids: Vec<String> = sets.iter().map(|set| display(&set.id_part)).collect();
            let message = format!(
                "Duplicate parametrization IDs detected, but strict_parametrization_ids is set.\n\
                 \n\
                 Test name:      {nodeid}\n\
                 Parameters:     {}\n\
                 Parameter sets: {}\n\
                 IDs:            {}\n\
                 Duplicates:     {}\n\
                 \n\
                 You can fix this problem using `@pytest.mark.parametrize(..., ids=...)` or `pytest.param(..., id=...)`.",
                argnames.join(", "),
                reprs.join(", "),
                ids.join(", "),
                duplicates.join(", "),
            );
            return Err(collect_error(py, &message));
        }
        if counts.get(&None).copied().unwrap_or(0) > 1 {
            let func_name = nodeid.rsplit("::").next().unwrap_or(nodeid);
            let msg = format!(
                "In {func_name}: multiple instances of HIDDEN_PARAM cannot be used in \
                 the same parametrize call, because the tests names need to be unique."
            );
            let failed_result: PyResult<PyErr> = (|| {
                let cls = py.import("_pytest.outcomes")?.getattr("Failed")?;
                let instance = cls.call1((&msg,))?;
                instance.setattr("pytrace", false)?;
                Ok(PyErr::from_value(instance))
            })();
            return Err(match failed_result {
                Ok(err) => err,
                Err(_) => collect_error(py, &msg),
            });
        }
        let mut existing: std::collections::HashSet<String> =
            sets.iter().filter_map(|set| set.id_part.clone()).collect();
        let mut suffixes: std::collections::HashMap<String, usize> =
            std::collections::HashMap::new();
        for set in sets.iter_mut() {
            let Some(id) = set.id_part.clone() else {
                continue;
            };
            if counts[&Some(id.clone())] <= 1 {
                continue;
            }
            let sep = if id.chars().last().is_some_and(|c| c.is_ascii_digit()) {
                "_"
            } else {
                ""
            };
            let counter = suffixes.entry(id.clone()).or_insert(0);
            let mut new_id = format!("{id}{sep}{counter}");
            while existing.contains(&new_id) {
                *counter += 1;
                new_id = format!("{id}{sep}{counter}");
            }
            existing.insert(new_id.clone());
            set.id_part = Some(new_id);
            *counter += 1;
        }
    }
    Ok(())
}

/// The single NOTSET parameter set pytest collects for empty argvalues,
/// carrying the configured empty_parameter_set_mark (default: skip).
fn notset_param_set(
    py: Python<'_>,
    argnames: &[String],
    func: Option<&Bound<'_, PyAny>>,
    indirect_all: bool,
    indirect_names: &[String],
) -> PyResult<ParamSet> {
    let mark_decorator = py
        .import("_pytest.mark")?
        .getattr("get_empty_parameterset_mark")?
        .call1((
            existing_py_config(py).unwrap_or_else(|| py.None()),
            argnames.to_vec(),
            func.map(|f| f.clone().unbind())
                .unwrap_or_else(|| py.None()),
        ))?;
    let mark_obj = mark_decorator.getattr("mark")?;
    let mark_name: String = mark_obj.getattr("name")?.extract()?;
    let notset = py.import("_pytest.compat")?.getattr("NOTSET")?;
    let mut params: Vec<(String, Py<PyAny>)> = Vec::new();
    let mut indirect_params: Vec<(String, usize, Py<PyAny>)> = Vec::new();
    for argname in argnames.iter().cloned() {
        if indirect_all || indirect_names.iter().any(|n| n == &argname) {
            indirect_params.push((argname, 0usize, notset.clone().unbind()));
        } else {
            params.push((argname, notset.clone().unbind()));
        }
    }
    Ok(ParamSet {
        id_part: Some("NOTSET".to_string()),
        params,
        indirect_params,
        extra_marks: vec![MarkData {
            name: mark_name,
            obj: mark_obj.unbind(),
        }],
    })
}

/// A pytest.Collector.CollectError carrying `message`: collection fails
/// with the message shown bare (no traceback) in the ERRORS section.
pub(crate) fn collect_error(py: Python<'_>, message: &str) -> PyErr {
    let cls = py
        .import("pytest")
        .and_then(|m| m.getattr("Collector"))
        .and_then(|c| c.getattr("CollectError"));
    match cls {
        Ok(cls) => match cls.call1((message,)) {
            Ok(instance) => PyErr::from_value(instance),
            Err(err) => err,
        },
        Err(err) => err,
    }
}

/// Some(message) when `err` is a CollectError or a Failed(pytrace=False),
/// shown without a traceback.
pub fn collect_error_message(py: Python<'_>, err: &PyErr) -> Option<String> {
    let collect_cls = py
        .import("pytest")
        .and_then(|m| m.getattr("Collector"))
        .and_then(|c| c.getattr("CollectError"))
        .ok()?;
    if err.matches(py, &collect_cls).unwrap_or(false) {
        return Some(err.value(py).to_string());
    }
    let failed_cls = py
        .import("_pytest.outcomes")
        .and_then(|m| m.getattr("Failed"))
        .ok()?;
    if err.matches(py, &failed_cls).unwrap_or(false) {
        let pytrace = err
            .value(py)
            .getattr("pytrace")
            .and_then(|v| v.extract::<bool>())
            .unwrap_or(true);
        if !pytrace {
            let msg = err
                .value(py)
                .getattr("msg")
                .and_then(|v| v.extract::<String>())
                .unwrap_or_else(|_| err.value(py).to_string());
            return Some(format!("E   Failed: {msg}"));
        }
    }
    None
}

/// pytest-style id for one parameter value.
/// The id object for one fixture param when @pytest.fixture(ids=...) was
/// given: ids[index] for a list, ids(value) for a callable. None (absent
/// ids, None entry, or error) falls back to the value-derived id.
pub(crate) fn fixture_param_id(
    py: Python<'_>,
    ids: Option<&Py<PyAny>>,
    value: &Bound<'_, PyAny>,
    index: usize,
) -> Option<Py<PyAny>> {
    let ids = ids?.bind(py);
    let id_obj = if ids.is_callable() {
        ids.call1((value,)).ok()?
    } else {
        ids.get_item(index).ok()?
    };
    if id_obj.is_none() {
        return None;
    }
    Some(id_obj.unbind())
}

/// pytest's ascii_escaped for str ids ("\x00" -> "\\x00"); printable
/// ASCII passes through untouched.
/// Upstream's _idval_from_value applied to a user-supplied id (an
/// `ids=` callable or list entry): strings ascii-escape, numbers/bools
/// stringify, anything else falls through to the default id (None).
pub(crate) fn user_id_from_value(py: Python<'_>, id: &Bound<'_, PyAny>) -> Option<String> {
    let _ = py;
    if id.is_none() {
        return None;
    }
    if let Ok(text) = id.extract::<String>() {
        return Some(ascii_escaped_str(id, text));
    }
    if id.extract::<bool>().is_ok() || id.extract::<i64>().is_ok() || id.extract::<f64>().is_ok() {
        return id.str().ok().map(|s| s.to_string());
    }
    None
}

pub(crate) fn ascii_escaped_str(value: &Bound<'_, PyAny>, s: String) -> String {
    // Pass printable ASCII through unchanged, but backslashes must be escaped
    // via unicode_escape (real pytest: "\\" → "\\\\") so node IDs are unambiguous.
    if s.chars().all(|c| matches!(c, ' '..='~')) && !s.contains('\\') {
        return s;
    }
    value
        .call_method1("encode", ("unicode_escape",))
        .and_then(|b| b.call_method1("decode", ("ascii",)))
        .and_then(|s| s.extract::<String>())
        .unwrap_or(s)
}

/// pytest's _idval: how one parametrize value renders in the test ID.
pub(crate) fn id_for_value(value: &Bound<'_, PyAny>, argname: &str, index: usize) -> String {
    if value.is_none() {
        return "None".to_string();
    }
    if let Ok(b) = value.cast::<pyo3::types::PyBool>() {
        return if b.is_true() { "True" } else { "False" }.to_string();
    }
    if let Ok(s) = value.extract::<String>() {
        return ascii_escaped_str(value, s);
    }
    // bytes: ascii_escaped = decode("ascii", "backslashreplace") with
    // non-printables escaped.
    if let Ok(bytes) = value.cast::<pyo3::types::PyBytes>() {
        return bytes
            .as_bytes()
            .iter()
            .map(|&b| {
                if matches!(b, 0x20..=0x7e) {
                    (b as char).to_string()
                } else {
                    format!("\\x{b:02x}")
                }
            })
            .collect();
    }
    // Numbers and enums all render via str() (upstream hits the number
    // branch first, so IntEnum is "30", plain Enum "Color.RED" — both str).
    let py = value.py();
    let is_enum = py
        .import("enum")
        .and_then(|m| m.getattr("Enum"))
        .and_then(|cls| value.is_instance(&cls))
        .unwrap_or(false);
    if (is_enum
        || value.cast::<pyo3::types::PyInt>().is_ok()
        || value.cast::<pyo3::types::PyFloat>().is_ok()
        || value.cast::<pyo3::types::PyComplex>().is_ok())
        && let Ok(s) = value.str()
    {
        return s.to_string();
    }
    // re.Pattern: the (escaped) pattern text.
    let is_pattern = py
        .import("re")
        .and_then(|m| m.getattr("Pattern"))
        .and_then(|cls| value.is_instance(&cls))
        .unwrap_or(false);
    if is_pattern
        && let Ok(pattern) = value.getattr("pattern")
        && let Ok(s) = pattern.extract::<String>()
    {
        return ascii_escaped_str(&pattern, s);
    }
    // Classes and functions render as their __name__.
    if let Ok(name) = value.getattr("__name__")
        && let Ok(s) = name.extract::<String>()
    {
        return s;
    }
    format!("{argname}{index}")
}

/// Read `pytestmark` from a function, class, or module. Accepts a single
/// mark or a list, and normalizes bare MarkDecorators (e.g.
/// `pytestmark = pytest.mark.asyncio`) to their Mark.
pub(crate) fn read_marks(py: Python<'_>, obj: &Bound<'_, PyAny>) -> PyResult<Vec<MarkData>> {
    // get_unpacked_marks (upstream): classes merge their whole MRO's own
    // pytestmark lists, base classes first; MarkDecorators unwrap to Marks.
    let mut marks = Vec::new();
    let get_unpacked = match py
        .import("pytest._marks")
        .and_then(|m| m.getattr("get_unpacked_marks"))
    {
        Ok(f) => f,
        Err(_) => return Ok(marks),
    };
    // Propagate TypeError (invalid pytestmark) so the caller can report it
    // as a collection error; swallow only import/getattr failures above.
    let entries = get_unpacked.call1((obj,))?;
    let Ok(iter) = entries.try_iter() else {
        return Ok(marks);
    };
    for mark in iter.flatten() {
        // Defensive: skip entries without a string name (stubs, mocks).
        let Ok(name) = mark.getattr("name").and_then(|n| n.extract::<String>()) else {
            continue;
        };
        marks.push(MarkData {
            name,
            obj: mark.unbind(),
        });
    }
    Ok(marks)
}

/// Expand `testpaths` ini globs against the rootdir (sorted per entry,
/// recursive ** supported), pytest's Config._decide_args.
pub fn glob_testpaths(py: Python<'_>, rootdir: &Path, entries: &[String]) -> PyResult<Vec<String>> {
    let glob = py.import("glob")?;
    let kwargs = pyo3::types::PyDict::new(py);
    kwargs.set_item("recursive", true)?;
    let mut out = Vec::new();
    for entry in entries {
        let pattern = rootdir.join(entry);
        let mut matches: Vec<String> = glob
            .call_method("glob", (pattern.to_string_lossy().as_ref(),), Some(&kwargs))?
            .extract()?;
        matches.sort();
        out.extend(matches);
    }
    Ok(out)
}

/// The -k matching name set for an item (upstream KeywordMatcher.from_item):
/// node-chain names (path components, class names, test name with params),
/// names assigned directly on the test function, and mark names.
pub fn keyword_match_names(py: Python<'_>, item: &TestItem) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    let (file_part, rest) = item
        .nodeid
        .split_once("::")
        .unwrap_or((item.nodeid.as_str(), ""));
    // Subdirectory and module-file names (upstream includes every chain
    // node below the root directory).
    for component in std::path::Path::new(file_part).components() {
        names.push(component.as_os_str().to_string_lossy().to_string());
    }
    for part in rest.split("::") {
        if !part.is_empty() {
            names.push(part.to_string());
        }
    }
    // Names attached to the function through direct assignment.
    if let Ok(dict) = item.func.bind(py).getattr("__dict__")
        && let Ok(keys) = dict.call_method0("keys")
        && let Ok(iter) = keys.try_iter()
    {
        for key in iter.flatten() {
            if let Ok(name) = key.extract::<String>() {
                names.push(name);
            }
        }
    }
    for mark in &item.marks {
        names.push(mark.name.clone());
    }
    // extra_keyword_matches set by pytest_pycollect_makeitem hooks on the class.
    if let Some(cls) = &item.cls
        && let Ok(extras) = cls
            .bind(py)
            .getattr("_pytest_extra_keyword_matches")
            .and_then(|v| v.extract::<Vec<String>>())
    {
        names.extend(extras);
    }
    names
}
