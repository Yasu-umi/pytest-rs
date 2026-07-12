#[allow(unused_imports)]
use super::super::*;
use crate::collect::{MarkData, TestItem};
use std::path::{Path, PathBuf};

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
    /// Files where a hook returned a bare File/Module (no real `collect()`
    /// override) — the caller re-collects these through the native
    /// module-scanning path instead of trusting the stub's empty yield.
    pub native_fallback: Vec<PathBuf>,
}

/// Collect items via pytest_collect_file hooks.
pub fn collect_custom_files(
    py: Python<'_>,
    rootdir: &Path,
    files: &[PathBuf],
    hooks: &[crate::session::PyHook],
    items: &mut Vec<TestItem>,
) -> PyResult<CustomCollectResult> {
    let mut skipped: Vec<(PathBuf, String)> = Vec::new();
    let mut collect_errors: Vec<(PathBuf, String)> = Vec::new();
    let mut native_fallback: Vec<PathBuf> = Vec::new();
    let Some(config) = crate::python::proxies::existing_py_config(py) else {
        return Ok(CustomCollectResult {
            skipped,
            errors: collect_errors,
            native_fallback,
        });
    };
    let config = config.bind(py);
    // pytest_collect_file impls live on the shim pluginmanager (autoloaded
    // plugin modules + objects registered at configure, e.g. pytest-mypy);
    // the hook relay reaches them all. Conftest-defined impls are excluded
    // from the relay and dispatched directly below instead (baseid-scoped
    // to the conftest's own directory) — otherwise e.g. a sub1/conftest.py
    // hook would fire for sub2's files too (test_pytest_collect_file_from_sister_dir).
    let pluginmanager = py
        .import("pytest._pluginmanager")?
        .getattr("pluginmanager")?;
    let collect_file = pluginmanager
        .getattr("hook")?
        .getattr("pytest_collect_file")?;
    let conftest_plugins = pluginmanager.getattr("_conftest_plugins")?;
    let conftest_collect_hooks: Vec<&crate::session::PyHook> = hooks
        .iter()
        .filter(|h| h.name == "pytest_collect_file" && h.plugin_module.is_none())
        .collect();
    let pathlib = py.import("pathlib")?.getattr("Path")?;
    let node_mod = py.import("pytest._node")?;
    let collector_cls = node_mod.getattr("Collector")?;
    let is_bare_file_collector = node_mod.getattr("is_bare_file_collector")?;
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
        // The relay returns a list of every non-conftest plugin's result
        // (collector|None); conftest impls are excluded here and dispatched
        // directly below with directory scoping.
        let kwargs = pyo3::types::PyDict::new(py);
        kwargs.set_item("file_path", &file_path)?;
        kwargs.set_item("parent", &parent)?;
        let results =
            match collect_file.call_method("call_excluding", (&conftest_plugins,), Some(&kwargs)) {
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
        let mut results_list: Vec<Bound<'_, PyAny>> = if results.is_none() {
            Vec::new()
        } else {
            results.try_iter()?.collect::<PyResult<_>>()?
        };
        // Conftest-defined pytest_collect_file impls only apply to files
        // under their own conftest's directory (baseid), mirroring
        // call_ignore_collect_hooks's scoping.
        let file_dir = file.parent().unwrap_or(rootdir);
        let mut hook_skipped = false;
        for hook in &conftest_collect_hooks {
            let hook_dir = if hook.baseid.is_empty() {
                rootdir.to_path_buf()
            } else {
                rootdir.join(&hook.baseid)
            };
            if !file_dir.starts_with(&hook_dir) {
                continue;
            }
            let result = call_py_hook_raw(
                py,
                &hook.func,
                &[
                    ("file_path", file_path.clone().unbind()),
                    ("parent", parent.clone().unbind()),
                ],
            );
            match result {
                Ok(r) => {
                    let r = r.bind(py);
                    if !r.is_none() {
                        results_list.push(r.clone());
                    }
                }
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
                    hook_skipped = true;
                    break;
                }
                Err(e) => return Err(e),
            }
        }
        if hook_skipped {
            continue;
        }
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
            // collector class (e.g. MyModule replacing the default Module) —
            // relabel first, regardless of whether collect() is a real
            // override: a hook matching broadly (e.g. every ".py" file) may
            // return a bare collector for a file the standard pipeline
            // already scanned natively, and those items just need their
            // display class updated, not re-collecting.
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
            // A bare File/Module (from_parent(...) with no collect() override)
            // always yields [] via the base stub — real scanning for
            // standard .py files happens natively in Rust and never calls
            // this method. Only queue the native fallback when this file
            // truly has no items yet (e.g. an unrecognized extension); a
            // file the standard pipeline already collected just needed the
            // relabel above.
            if pre_existing.is_empty()
                && is_bare_file_collector
                    .call1((&collector,))?
                    .extract::<bool>()?
            {
                native_fallback.push(file.clone());
                continue;
            }
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
                let func_class: String = item_obj
                    .getattr("__class__")
                    .and_then(|c| c.getattr("__name__"))
                    .and_then(|n| n.extract())
                    .unwrap_or_else(|_| String::new());
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
                    func_class,
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
        native_fallback,
    })
}
