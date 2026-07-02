//! Conftest/plugin hook firing (configure, sessionstart/finish, modifyitems).

#[allow(unused_imports)]
use super::*;
use crate::hooks::HookContext;
use crate::python;

impl Engine {
    pub(crate) fn fire_configure(&mut self, py: Python<'_>) -> PyResult<()> {
        let mut ctx = HookContext {
            py,
            session: &mut self.session,
            config: &self.config,
        };
        for plugin in self.plugins.iter_mut() {
            plugin.pytest_configure(&mut ctx)?;
        }
        Ok(())
    }

    pub(crate) fn fire_plugins_registered(&mut self, py: Python<'_>) -> PyResult<()> {
        let mut ctx = HookContext {
            py,
            session: &mut self.session,
            config: &self.config,
        };
        for plugin in self.plugins.iter_mut() {
            plugin.pytest_plugins_registered(&mut ctx)?;
        }
        Ok(())
    }

    pub(crate) fn fire_sessionstart(&mut self, py: Python<'_>) -> PyResult<()> {
        let mut ctx = HookContext {
            py,
            session: &mut self.session,
            config: &self.config,
        };
        for plugin in self.plugins.iter_mut() {
            plugin.pytest_sessionstart(&mut ctx)?;
        }
        Ok(())
    }

    pub(crate) fn fire_collection_modifyitems(&mut self, py: Python<'_>) -> PyResult<()> {
        // Temporarily move items out so hooks can mutate the list while the
        // session stays borrowable.
        let mut items = std::mem::take(&mut self.session.items);
        // conftest hooks run before bundled-plugin hooks (pluggy LIFO:
        // later-registered conftest hookimpls fire first), so marks added
        // programmatically are visible to the plugins (e.g. anyio's
        // marker-driven backend parametrization, issue #422 upstream).
        if let Err(err) = self.run_py_modifyitems(py, &mut items) {
            self.session.items = items;
            return Err(err);
        }
        // -k/-m deselection runs after the conftest hooks (so dynamically
        // added markers are visible, upstream's trylast hookimpl) but
        // before the bundled plugins' hooks (pytest-split must split the
        // already-deselected set).
        self.session.items = items;
        self.apply_selection(py)?;
        let mut items = std::mem::take(&mut self.session.items);
        {
            let mut ctx = HookContext {
                py,
                session: &mut self.session,
                config: &self.config,
            };
            for plugin in &self.plugins {
                if let Err(err) = plugin.pytest_collection_modifyitems(&mut ctx, &mut items) {
                    self.session.items = items;
                    return Err(err);
                }
            }
        }
        self.session.items = items;
        Ok(())
    }

    /// conftest pytest_collection_modifyitems hooks: items are exposed as
    /// node proxies; reordering, deselection, and added markers are read
    /// back from the proxy list.
    pub(crate) fn run_py_modifyitems(
        &mut self,
        py: Python<'_>,
        items: &mut Vec<crate::collect::TestItem>,
    ) -> PyResult<()> {
        let hook_for = |name: &str| -> Vec<Py<pyo3::PyAny>> {
            self.session
                .py_hooks
                .iter()
                .filter(|hook| hook.name == name)
                .map(|hook| hook.func.clone_ref(py))
                .collect()
        };
        let mut hook_funcs = hook_for("pytest_collection_modifyitems");
        // Plugins registered at configure time via pluginmanager.register()
        // (e.g. pytest-order's OrderingPlugin) live in pluginmanager._plugins,
        // not session.py_hooks — include their impls too.
        hook_funcs.extend(python::instance_hook_funcs(
            py,
            "pytest_collection_modifyitems",
        ));
        let itemcollected_funcs = hook_for("pytest_itemcollected");
        let collectstart_funcs = hook_for("pytest_collectstart");
        let recording = crate::engine::inprocess::recording();
        if hook_funcs.is_empty()
            && itemcollected_funcs.is_empty()
            && collectstart_funcs.is_empty()
            && !recording
        {
            return Ok(());
        }

        let config_proxy = python::make_py_config(py, &self.config)?;
        let nodes: Vec<Py<pyo3::PyAny>> = items
            .iter()
            .map(|item| python::make_node(py, item))
            .collect::<PyResult<_>>()?;
        let node_list = pyo3::types::PyList::new(py, nodes.iter().map(|n| n.bind(py)))?;

        // pytest_collectstart: fire for Session proxy first (even with 0
        // items), then per distinct test class.
        if !collectstart_funcs.is_empty() {
            let node_mod = py.import("pytest._node")?;
            let proxy_cls = node_mod.getattr("_CollectorProxy")?;
            let rootdir_str = self.config.rootdir.to_string_lossy().to_string();
            let py_config = python::existing_py_config(py);
            let session_node = node_mod.getattr("_NodeSession")?.call1((py_config,))?;
            let session_collector = proxy_cls.call1((
                "",
                "",
                rootdir_str.as_str(),
                session_node,
                py.None(),
                "Session",
            ))?;
            for func in &collectstart_funcs {
                python::call_py_hook(
                    py,
                    func,
                    &[("collector", session_collector.clone().unbind())],
                )?;
            }
            let class_cls = node_mod.getattr("Class")?;
            let mut seen_cls: Vec<Py<pyo3::PyAny>> = Vec::new();
            for node in node_list.iter() {
                let cls = node.getattr("cls").ok().filter(|c| !c.is_none());
                let Some(cls) = cls else { continue };
                if seen_cls.iter().any(|c| c.bind(py).is(&cls)) {
                    continue;
                }
                seen_cls.push(cls.clone().unbind());
                let kw = pyo3::types::PyDict::new(py);
                kw.set_item("obj", &cls)?;
                kw.set_item("config", config_proxy.clone_ref(py))?;
                kw.set_item("name", cls.getattr("__name__").ok())?;
                let collector = class_cls.call((), Some(&kw))?;
                for func in &collectstart_funcs {
                    python::call_py_hook(py, func, &[("collector", collector.clone().unbind())])?;
                }
                // Propagate the collector's markers to this class's items.
                let class_marks: Vec<Py<pyo3::PyAny>> = collector
                    .getattr("own_markers")?
                    .try_iter()?
                    .filter_map(|m| m.ok().map(|m| m.unbind()))
                    .collect();
                if !class_marks.is_empty() {
                    for item_node in node_list.iter() {
                        let item_cls = item_node.getattr("cls").ok().filter(|c| !c.is_none());
                        if item_cls.is_some_and(|c| c.is(&cls)) {
                            let own = item_node.getattr("own_markers")?;
                            for mark in &class_marks {
                                own.call_method1("append", (mark.bind(py),))?;
                            }
                        }
                    }
                }
            }
        }

        // pytest_itemcollected per item (Django method tags -> marks).
        for func in &itemcollected_funcs {
            for node in node_list.iter() {
                python::call_py_hook(py, func, &[("item", node.clone().unbind())])?;
            }
        }

        for func in &hook_funcs {
            python::call_py_hook(
                py,
                func,
                &[
                    ("config", config_proxy.clone_ref(py)),
                    ("items", node_list.clone().unbind().into_any()),
                    ("session", python::make_session_proxy(py, &self.config)?),
                ],
            )?;
        }

        // In a nested run, surface the collector tree, per-item itemcollected,
        // and the single modifyitems call to the HookRecorder so that
        // getcalls/getreports work as they do in real pytest.
        if recording {
            if let Err(err) = self.record_collector_tree(py, items) {
                eprintln!(
                    "INTERNAL ERROR: record_collector_tree: {}",
                    python::format_exception(py, &err)
                );
            }
            for node in node_list.iter() {
                python::record_hook(
                    py,
                    "pytest_itemcollected",
                    &[("item", node.clone().unbind())],
                );
            }
            python::record_hook(
                py,
                "pytest_collection_modifyitems",
                &[
                    ("config", config_proxy.clone_ref(py)),
                    ("items", node_list.clone().unbind().into_any()),
                    ("session", python::make_session_proxy(py, &self.config)?),
                ],
            );
        }

        // session.add_marker() calls store in _session_state["session_markers"]; append
        // them to every node's own_markers so -m and request.keywords both see them.
        let session_markers: Vec<Bound<'_, PyAny>> = py
            .import("pytest._node")?
            .call_method0("get_session_markers")?
            .try_iter()?
            .filter_map(|m| m.ok())
            .collect();
        if !session_markers.is_empty() {
            for node in node_list.iter() {
                let own = node.getattr("own_markers")?;
                for marker in &session_markers {
                    own.call_method1("append", (marker,))?;
                }
            }
        }

        // Read back order/membership (by nodeid) and any added markers.
        // Use VecDeque per nodeid to correctly handle --keep-duplicates
        // where the same nodeid appears multiple times in the item list.
        let mut by_nodeid: std::collections::HashMap<
            String,
            std::collections::VecDeque<crate::collect::TestItem>,
        > = Default::default();
        for item in std::mem::take(items) {
            by_nodeid
                .entry(item.nodeid.clone())
                .or_default()
                .push_back(item);
        }
        for node in node_list.iter() {
            let nodeid: String = node.getattr("nodeid")?.extract()?;
            if let Some(queue) = by_nodeid.get_mut(&nodeid)
                && let Some(mut item) = queue.pop_front()
            {
                let mut marks = Vec::new();
                for mark in node.getattr("own_markers")?.try_iter()? {
                    let mark = mark?;
                    marks.push(crate::collect::MarkData {
                        name: mark.getattr("name")?.extract()?,
                        obj: mark.unbind(),
                    });
                }
                item.marks = marks;
                items.push(item);
            }
        }
        Ok(())
    }

    /// Emit pytest_collectstart + pytest_make_collect_report +
    /// pytest_collectreport triples for the full collector tree to the
    /// in-process HookRecorder.  Called only when recording() is true
    /// (inside a nested run).
    ///
    /// Uses `_CollectorProxy` objects so hook callers see real collector
    /// attributes (path, session, parent chain, __class__.__name__).
    /// The failed-module collectreport is already in the recorder (emitted
    /// by `reporter_collect_error` in handle_collection_errors), so we only
    /// emit its collectstart here.
    fn record_collector_tree(
        &self,
        py: Python<'_>,
        items: &[crate::collect::TestItem],
    ) -> PyResult<()> {
        use pyo3::types::{PyDict, PyList};
        let collect_report_cls = py.import("_pytest.reports")?.getattr("CollectReport")?;
        let simple_ns = py.import("types")?.getattr("SimpleNamespace")?;
        let node_mod = py.import("pytest._node")?;
        let proxy_cls = node_mod.getattr("_CollectorProxy")?;
        let pathlib_path = py.import("pathlib")?.getattr("Path")?;

        let py_config = python::existing_py_config(py);
        let session_proxy: Py<PyAny> = node_mod
            .getattr("_NodeSession")?
            .call1((py_config.as_ref().map(|c| c.bind(py).clone()),))?
            .unbind();

        // ── Helpers ──────────────────────────────────────────────────────

        let make_proxy = |name: &str,
                          nodeid: &str,
                          path_str: &str,
                          parent: Py<PyAny>,
                          class_name: &str|
         -> PyResult<Py<PyAny>> {
            let py_path = pathlib_path
                .call1((path_str,))?
                .call_method0("resolve")
                .unwrap_or_else(|_| pathlib_path.call1((path_str,)).unwrap());
            proxy_cls
                .call1((
                    name,
                    nodeid,
                    py_path,
                    session_proxy.clone_ref(py),
                    parent,
                    class_name,
                ))
                .map(|b| b.unbind())
        };

        let dir_key = |file: &str| -> String {
            match std::path::Path::new(file).parent() {
                Some(p) if p.as_os_str().is_empty() => ".".to_string(),
                Some(p) => p.to_string_lossy().into_owned(),
                None => ".".to_string(),
            }
        };

        let make_report = |nodeid: &str,
                           outcome: &str,
                           longrepr: Py<PyAny>,
                           result: &Bound<'_, PyList>|
         -> PyResult<Py<PyAny>> {
            let kw = PyDict::new(py);
            kw.set_item("nodeid", nodeid)?;
            kw.set_item("outcome", outcome)?;
            kw.set_item("longrepr", longrepr)?;
            let file = nodeid.split("::").next().unwrap_or(nodeid);
            kw.set_item("location", (file, py.None(), file))?;
            kw.set_item("result", result)?;
            kw.set_item("sections", PyList::empty(py))?;
            kw.set_item("when", "collect")?;
            collect_report_cls.call((), Some(&kw)).map(|b| b.unbind())
        };

        let fire_collector = |collector: &Py<PyAny>, report: Py<PyAny>| {
            python::record_hook(
                py,
                "pytest_collectstart",
                &[("collector", collector.clone_ref(py))],
            );
            python::record_hook(
                py,
                "pytest_make_collect_report",
                &[
                    ("collector", collector.clone_ref(py)),
                    ("report", report.clone_ref(py)),
                ],
            );
            python::record_hook(py, "pytest_collectreport", &[("report", report)]);
        };

        let item_stub = |nodeid: &str, name: &str| -> PyResult<Py<PyAny>> {
            let ns = PyDict::new(py);
            ns.set_item("name", name)?;
            ns.set_item("nodeid", nodeid)?;
            ns.set_item("fspath", py.None())?;
            ns.set_item("path", py.None())?;
            simple_ns.call((), Some(&ns)).map(|b| b.unbind())
        };

        // ── Data collection ──────────────────────────────────────────────

        let mut passing_modules: Vec<String> = Vec::new();
        {
            let mut seen: std::collections::HashSet<String> = Default::default();
            for item in items {
                let file = item
                    .nodeid
                    .split("::")
                    .next()
                    .unwrap_or(&item.nodeid)
                    .to_string();
                if seen.insert(file.clone()) {
                    passing_modules.push(file);
                }
            }
        }

        let failing_modules: Vec<&str> = self
            .session
            .collect_errors
            .iter()
            .map(|(nodeid, _)| nodeid.as_str())
            .collect();

        let skipped_modules: Vec<(&str, &str, &str)> = self
            .session
            .skipped_modules
            .iter()
            .map(|(nodeid, reason, loc)| (nodeid.as_str(), reason.as_str(), loc.as_str()))
            .collect();

        let collect_file_skips: Vec<(String, &str)> = self
            .session
            .collect_file_skips
            .iter()
            .map(|(nodeid, reason)| (dir_key(nodeid.as_str()), reason.as_str()))
            .collect();

        let mut dirs: Vec<String> = Vec::new();
        {
            let mut seen: std::collections::HashSet<String> = Default::default();
            seen.insert(".".to_string());
            dirs.push(".".to_string());
            let all_files = passing_modules
                .iter()
                .map(|s| s.as_str())
                .chain(failing_modules.iter().copied())
                .chain(skipped_modules.iter().map(|(nodeid, _, _)| *nodeid));
            for file in all_files {
                let dir = dir_key(file);
                if seen.insert(dir.clone()) {
                    dirs.push(dir);
                }
            }
            for (dir, _) in &collect_file_skips {
                if seen.insert(dir.clone()) {
                    dirs.push(dir.clone());
                }
            }
        }

        let skipped_dirs: std::collections::HashMap<String, String> = {
            let mut dir_has_items: std::collections::HashSet<String> = Default::default();
            for file in &passing_modules {
                dir_has_items.insert(dir_key(file.as_str()));
            }
            for nodeid in &failing_modules {
                dir_has_items.insert(dir_key(nodeid));
            }
            let mut skip_map: std::collections::HashMap<String, String> = Default::default();
            for (dir, reason) in &collect_file_skips {
                if !dir_has_items.contains(dir.as_str()) {
                    skip_map
                        .entry(dir.clone())
                        .or_insert_with(|| reason.to_string());
                }
            }
            skip_map
        };

        let mut classes: Vec<String> = Vec::new();
        {
            let mut seen: std::collections::HashSet<String> = Default::default();
            for item in items {
                let mut parts = item.nodeid.splitn(3, "::");
                if let (Some(file), Some(cls), Some(_)) = (parts.next(), parts.next(), parts.next())
                {
                    let key = format!("{}::{}", file, cls);
                    if seen.insert(key.clone()) {
                        classes.push(key);
                    }
                }
            }
        }

        // ── Build proxies (Vec for deterministic order) ──────────────────

        let rootdir_str = self.config.rootdir.to_string_lossy().to_string();
        let session: Py<PyAny> = make_proxy("", "", &rootdir_str, py.None(), "Session")?;

        let mut dir_proxies: Vec<(String, Py<PyAny>)> = Vec::new();
        for dir in &dirs {
            if let Ok(d) = make_proxy(dir, dir, dir, session.clone_ref(py), "Dir") {
                dir_proxies.push((dir.clone(), d));
            }
        }

        let mut mod_proxies: Vec<(String, Py<PyAny>)> = Vec::new();
        for file in &passing_modules {
            let pk = dir_key(file.as_str());
            let parent = dir_proxies
                .iter()
                .find(|(k, _)| *k == pk)
                .map(|(_, p)| p.clone_ref(py))
                .unwrap_or_else(|| session.clone_ref(py));
            let name = std::path::Path::new(file)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(file);
            if let Ok(m) = make_proxy(name, file, file, parent, "Module") {
                mod_proxies.push((file.clone(), m));
            }
        }

        let mut class_proxies: Vec<(String, Py<PyAny>)> = Vec::new();
        for key in &classes {
            let (file_part, cls_name) = key.split_once("::").unwrap_or((key, ""));
            let parent = mod_proxies
                .iter()
                .find(|(k, _)| *k == file_part)
                .map(|(_, p)| p.clone_ref(py))
                .unwrap_or_else(|| session.clone_ref(py));
            if let Ok(c) = make_proxy(cls_name, key, ".", parent, "Class") {
                class_proxies.push((key.clone(), c));
            }
        }

        // ── Emit collector tree ──────────────────────────────────────────

        // Session: collectstart + make_collect_report + collectreport
        {
            let children = PyList::empty(py);
            for (_, dp) in &dir_proxies {
                let _ = children.append(dp.bind(py));
            }
            let report = make_report("", "passed", py.None(), &children)?;
            let _ = report.bind(py).setattr("collector", session.bind(py));
            fire_collector(&session, report);
        }

        // Classes: collectstart + make_collect_report + collectreport
        for (key, cp) in &class_proxies {
            let children = PyList::empty(py);
            let prefix = format!("{}::", key);
            for item in items {
                if let Some(tail) = item.nodeid.strip_prefix(prefix.as_str())
                    && !tail.contains("::")
                    && let Ok(s) = item_stub(&item.nodeid, tail)
                {
                    let _ = children.append(s.bind(py));
                }
            }
            let report = make_report(key, "passed", py.None(), &children)?;
            let _ = report.bind(py).setattr("collector", cp.bind(py));
            fire_collector(cp, report);
        }

        // Passing modules: collectstart + make_collect_report + pycollect_makeitem* + collectreport
        for (file, mp) in &mod_proxies {
            let children = PyList::empty(py);
            let mut makeitem_names: Vec<String> = Vec::new();
            for (key, cp) in &class_proxies {
                if key.split_once("::").map(|(f, _)| f) == Some(file.as_str()) {
                    let _ = children.append(cp.bind(py));
                    if let Some(cls_name) = key.split_once("::").map(|(_, c)| c) {
                        makeitem_names.push(cls_name.to_string());
                    }
                }
            }
            for item in items {
                let item_file = item.nodeid.split("::").next().unwrap_or("");
                if item_file == file.as_str() {
                    let parts: Vec<_> = item.nodeid.splitn(3, "::").collect();
                    if parts.len() < 3 {
                        let nm = item.nodeid.rsplit("::").next().unwrap_or(&item.nodeid);
                        if let Ok(s) = item_stub(&item.nodeid, nm) {
                            let _ = children.append(s.bind(py));
                        }
                        makeitem_names.push(nm.to_string());
                    }
                }
            }
            let report = make_report(file, "passed", py.None(), &children)?;
            let _ = report.bind(py).setattr("collector", mp.bind(py));
            python::record_hook(
                py,
                "pytest_collectstart",
                &[("collector", mp.clone_ref(py))],
            );
            python::record_hook(
                py,
                "pytest_make_collect_report",
                &[
                    ("collector", mp.clone_ref(py)),
                    ("report", report.clone_ref(py)),
                ],
            );
            for name in &makeitem_names {
                let name_py: Py<PyAny> = pyo3::types::PyString::new(py, name).into_any().unbind();
                python::record_hook(
                    py,
                    "pytest_pycollect_makeitem",
                    &[
                        ("collector", mp.clone_ref(py)),
                        ("name", name_py),
                        ("obj", py.None()),
                    ],
                );
            }
            python::record_hook(py, "pytest_collectreport", &[("report", report)]);
        }

        // Skipped modules: collectstart + make_collect_report + collectreport(skipped)
        for (nodeid, reason, location) in &skipped_modules {
            let pk = dir_key(nodeid);
            let parent = dir_proxies
                .iter()
                .find(|(k, _)| *k == pk)
                .map(|(_, p)| p.clone_ref(py))
                .unwrap_or_else(|| session.clone_ref(py));
            let name = std::path::Path::new(nodeid)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(nodeid);
            if let Ok(mp) = make_proxy(name, nodeid, nodeid, parent, "Module") {
                let (loc_file, lineno) = if let Some(colon) = location.rfind(':') {
                    let f = &location[..colon];
                    let ln: u64 = location[colon + 1..].parse().unwrap_or(1);
                    (f, ln)
                } else {
                    (*location, 1u64)
                };
                let skip_reason = format!("Skipped: {reason}");
                let longrepr: Py<PyAny> = pyo3::types::PyTuple::new(
                    py,
                    [
                        loc_file.into_pyobject(py)?.into_any().unbind(),
                        lineno.into_pyobject(py)?.into_any().unbind(),
                        skip_reason.into_pyobject(py)?.into_any().unbind(),
                    ],
                )?
                .unbind()
                .into();
                let report = make_report(nodeid, "skipped", longrepr, &PyList::empty(py))?;
                let _ = report.bind(py).setattr("collector", mp.bind(py));
                fire_collector(&mp, report);
            }
        }

        // Failing modules: only collectstart (report already emitted).
        for nodeid in &failing_modules {
            let pk = dir_key(nodeid);
            let parent = dir_proxies
                .iter()
                .find(|(k, _)| *k == pk)
                .map(|(_, p)| p.clone_ref(py))
                .unwrap_or_else(|| session.clone_ref(py));
            let name = std::path::Path::new(nodeid)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(nodeid);
            if let Ok(mp) = make_proxy(name, nodeid, nodeid, parent, "Module") {
                python::record_hook(py, "pytest_collectstart", &[("collector", mp)]);
            }
        }

        // Dirs: collectstart + make_collect_report + collectreport
        for (dk, dp) in &dir_proxies {
            let children = PyList::empty(py);
            for (file, mp) in &mod_proxies {
                if dir_key(file.as_str()) == *dk {
                    let _ = children.append(mp.bind(py));
                }
            }
            if let Some(reason) = skipped_dirs.get(dk.as_str()) {
                let longrepr: Py<PyAny> = pyo3::types::PyTuple::new(
                    py,
                    [
                        dk.as_str().into_pyobject(py)?.into_any().unbind(),
                        1u64.into_pyobject(py)?.into_any().unbind(),
                        format!("Skipped: {reason}")
                            .into_pyobject(py)?
                            .into_any()
                            .unbind(),
                    ],
                )?
                .unbind()
                .into();
                let report = make_report(dk, "skipped", longrepr, &children)?;
                let _ = report.bind(py).setattr("collector", dp.bind(py));
                fire_collector(dp, report);
            } else {
                let report = make_report(dk, "passed", py.None(), &children)?;
                let _ = report.bind(py).setattr("collector", dp.bind(py));
                fire_collector(dp, report);
            }
        }

        Ok(())
    }

    /// pytest_deselected conftest/plugin hooks: called once with every item
    /// dropped by -k/-m/--lf selection (a copy, like pytest's list).
    pub(crate) fn fire_py_deselected(&mut self, py: Python<'_>) -> PyResult<()> {
        if self.session.deselected_items.is_empty() {
            return Ok(());
        }
        let mut hook_funcs = python::instance_hook_funcs(py, "pytest_deselected");
        hook_funcs.extend(
            self.session
                .py_hooks
                .iter()
                .filter(|hook| hook.name == "pytest_deselected")
                .map(|hook| hook.func.clone_ref(py)),
        );
        let delegated = self.session.custom_reporter.is_some();
        let recording = crate::engine::inprocess::recording();
        if hook_funcs.is_empty() && !delegated && !recording {
            return Ok(());
        }
        let nodes: Vec<Py<pyo3::PyAny>> = self
            .session
            .deselected_items
            .iter()
            .map(|item| python::make_node(py, item))
            .collect::<PyResult<_>>()?;
        let node_list = pyo3::types::PyList::new(py, nodes.iter().map(|n| n.bind(py)))?;
        for func in &hook_funcs {
            python::call_py_hook(
                py,
                func,
                &[("items", node_list.clone().unbind().into_any())],
            )?;
        }
        // In a nested run, surface the call to the HookRecorder even when no
        // conftest impl exists (pytest always dispatches it through pluggy).
        python::record_hook(
            py,
            "pytest_deselected",
            &[("items", node_list.clone().unbind().into_any())],
        );
        // The replacement reporter's own pytest_deselected hookimpl (stats
        // bookkeeping behind "X deselected" in its summary).
        if delegated {
            python::reporter_deselected(py, node_list.as_any());
        }
        Ok(())
    }

    /// Fire conftest hooks that only take `config` (e.g. pytest_configure).
    /// pytest_addoption(parser) for python plugins/conftests: the shim
    /// parser records option/ini specs so config.getoption()/getini() can
    /// resolve plugin-declared defaults (full CLI parsing stays Rust-side).
    pub(crate) fn fire_py_addoption_hooks(&mut self, py: Python<'_>) -> PyResult<()> {
        let hook_funcs: Vec<Py<pyo3::PyAny>> = self
            .session
            .py_hooks
            .iter()
            .filter(|hook| hook.name == "pytest_addoption")
            .map(|hook| hook.func.clone_ref(py))
            .collect();
        if hook_funcs.is_empty() {
            return Ok(());
        }
        let parser = py.import("pytest._parser")?.getattr("parser")?.unbind();
        // Upstream signature: pytest_addoption(parser, pluginmanager) —
        // call_py_hook only passes what each impl's signature requests.
        let pluginmanager = py
            .import("pytest._pluginmanager")?
            .getattr("pluginmanager")?
            .unbind();
        for func in &hook_funcs {
            python::call_py_hook(
                py,
                func,
                &[
                    ("parser", parser.clone_ref(py)),
                    ("pluginmanager", pluginmanager.clone_ref(py)),
                ],
            )?;
        }
        Ok(())
    }

    /// pytest_load_initial_conftests(early_config, parser, args): pytest's
    /// early hook, after option specs are registered (so getini works) and
    /// before configure. pytest-env reads getini("env") here to set os.environ.
    pub(crate) fn fire_py_load_initial_conftests(&mut self, py: Python<'_>) -> PyResult<()> {
        // Respect tryfirst/trylast ordering (same as fire_py_hooks_simple).
        let mut sorted_hooks: Vec<_> = self
            .session
            .py_hooks
            .iter()
            .filter(|hook| hook.name == "pytest_load_initial_conftests")
            .collect();
        sorted_hooks.sort_by_key(|h| match (h.tryfirst, h.trylast) {
            (true, _) => 0,
            (_, true) => 2,
            _ => 1,
        });
        let hook_funcs: Vec<Py<pyo3::PyAny>> =
            sorted_hooks.iter().map(|h| h.func.clone_ref(py)).collect();
        if hook_funcs.is_empty() {
            return Ok(());
        }
        let early_config = python::make_py_config(py, &self.config)?;
        let parser = py.import("pytest._parser")?.getattr("parser")?.unbind();
        // Upstream passes the full invocation args so plugins can call
        // parser.parse_known_args(args) to find plugin-defined flags like --ds.
        // We reconstruct it as plugin_args (unknown --flags) + paths (positionals).
        let full_args: Vec<&str> = self
            .config
            .plugin_args
            .iter()
            .map(String::as_str)
            .chain(self.config.paths.iter().map(String::as_str))
            .collect();
        let args = pyo3::types::PyList::new(py, &full_args)?
            .into_any()
            .unbind();
        for func in &hook_funcs {
            python::call_py_hook(
                py,
                func,
                &[
                    ("early_config", early_config.clone_ref(py)),
                    ("parser", parser.clone_ref(py)),
                    ("args", args.clone_ref(py)),
                ],
            )?;
        }
        Ok(())
    }

    /// Deferred `--flag[=value]` tokens (unknown to clap) resolve against
    /// the python-plugin option specs onto config.option; unregistered
    /// leftovers usage-error like pytest's "unrecognized arguments".
    pub(crate) fn apply_plugin_cli_args(&mut self, py: Python<'_>) -> PyResult<()> {
        if self.config.plugin_args.is_empty() {
            return Ok(());
        }
        let config_proxy = python::make_py_config(py, &self.config)?;
        let option = config_proxy.bind(py).getattr("option")?;
        let (unknown, positionals): (Vec<String>, Vec<String>) = py
            .import("pytest._parser")?
            .call_method1("apply_cli_args", (option, self.config.plugin_args.clone()))?
            .extract()?;
        if !positionals.is_empty() {
            self.config.paths.extend(positionals);
        }
        if !unknown.is_empty() {
            // Match argparse/pytest's MyOptionParser.error: a
            // "<prog>: error: <message>" line followed by the sorted
            // extra_info (inifile, rootdir) pytest attaches to the parser.
            let mut msg = format!(
                "pytest: error: unrecognized arguments: {}",
                unknown.join(" ")
            );
            if let Some(name) = &self.config.config_file_name {
                let inifile = self.config.rootdir.join(name);
                msg += &format!("\n  inifile: {}", inifile.display());
            }
            msg += &format!("\n  rootdir: {}", self.config.rootdir.display());
            return Err(python::usage_error(py, &msg));
        }
        Ok(())
    }

    pub(crate) fn fire_py_hooks_simple(&mut self, py: Python<'_>, name: &str) -> PyResult<()> {
        // Respect @pytest.hookimpl(tryfirst/trylast) ordering: tryfirst before
        // normal, normal next, trylast last (mirrors pluggy's call ordering).
        let mut hooks: Vec<_> = self
            .session
            .py_hooks
            .iter()
            .filter(|hook| hook.name == name)
            .collect();
        hooks.sort_by_key(|h| match (h.tryfirst, h.trylast) {
            (true, _) => 0,
            (_, true) => 2,
            _ => 1,
        });
        let hook_funcs: Vec<Py<pyo3::PyAny>> = hooks.iter().map(|h| h.func.clone_ref(py)).collect();
        // pluggy fires the HookCaller even with zero implementations, so an
        // in-process HookRecorder records the (empty) call regardless. Skip
        // only on the outer run when there is nothing to call and no recorder
        // to notify.
        if hook_funcs.is_empty() && !crate::engine::inprocess::recording() {
            return Ok(());
        }
        let config_proxy = python::make_py_config(py, &self.config)?;
        // Record the hook call so an in-process HookRecorder's getcalls sees
        // it (the native engine dispatches conftest/plugin hooks directly,
        // bypassing pluggy's HookCaller that the monitoring wraps).
        python::record_hook(py, name, &[("config", config_proxy.clone_ref(py))]);
        for func in &hook_funcs {
            python::call_py_hook(py, func, &[("config", config_proxy.clone_ref(py))])?;
        }
        Ok(())
    }

    pub(crate) fn fire_sessionfinish(&mut self, py: Python<'_>, code: i32) -> PyResult<()> {
        let mut ctx = HookContext {
            py,
            session: &mut self.session,
            config: &self.config,
        };
        for plugin in self.plugins.iter_mut() {
            plugin.pytest_sessionfinish(&mut ctx, code)?;
        }
        self.fire_py_sessionfinish(py, code)
    }

    /// pytest_sessionstart conftest/plugin hooks (sugar reads its theme
    /// config here, pretty stamps its wall-clock start).
    pub(crate) fn fire_py_sessionstart(&mut self, py: Python<'_>) -> PyResult<()> {
        let hook_funcs: Vec<Py<pyo3::PyAny>> = self
            .session
            .py_hooks
            .iter()
            .filter(|hook| hook.name == "pytest_sessionstart")
            .map(|hook| hook.func.clone_ref(py))
            .collect();
        if hook_funcs.is_empty() {
            return Ok(());
        }
        let session = python::make_session_proxy(py, &self.config)?;
        for func in &hook_funcs {
            python::call_py_hook(py, func, &[("session", session.clone_ref(py))])?;
        }
        Ok(())
    }

    /// pytest_collection_finish conftest/plugin hooks, with the final item
    /// set on session.items (sugar's tests_count comes from here).
    pub(crate) fn fire_py_collection_finish(&mut self, py: Python<'_>) -> PyResult<()> {
        // Instance-registered plugin impls first (pytest-run-parallel's
        // runner is tryfirst: it wraps item.obj before reporters look).
        let mut hook_funcs = python::instance_hook_funcs(py, "pytest_collection_finish");
        hook_funcs.extend(
            self.session
                .py_hooks
                .iter()
                .filter(|hook| hook.name == "pytest_collection_finish")
                .map(|hook| hook.func.clone_ref(py)),
        );
        // Publish session.items / session.testscollected regardless of
        // hook presence: pytest_sessionfinish readers need them too.
        python::set_session_items(py, &self.session.items)?;
        python::set_session_skipped_modules(py, &self.session.skipped_modules)?;
        if hook_funcs.is_empty()
            && self.session.custom_reporter.is_none()
            && !crate::engine::inprocess::recording()
        {
            return Ok(());
        }
        let session = python::make_session_proxy(py, &self.config)?;
        python::record_hook(
            py,
            "pytest_collection_finish",
            &[("session", session.clone_ref(py))],
        );
        for func in &hook_funcs {
            python::call_py_hook(py, func, &[("session", session.clone_ref(py))])?;
        }
        // Plugins may swap item.obj on the published items here
        // (pytest-run-parallel wraps test functions for threaded repeats).
        python::apply_session_obj_overrides(py, &mut self.session.items)?;
        Ok(())
    }

    /// pytest_sessionfinish conftest/plugin hooks (session is not modeled;
    /// hooks asking for it receive None).
    pub(crate) fn fire_py_sessionfinish(&mut self, py: Python<'_>, code: i32) -> PyResult<()> {
        let mut hook_funcs = python::instance_hook_funcs(py, "pytest_sessionfinish");
        hook_funcs.extend(
            self.session
                .py_hooks
                .iter()
                .filter(|hook| hook.name == "pytest_sessionfinish")
                .map(|hook| hook.func.clone_ref(py)),
        );
        if hook_funcs.is_empty() {
            return Ok(());
        }
        let config_proxy = python::make_py_config(py, &self.config)?;
        let session_proxy = python::make_session_proxy(py, &self.config)?;
        let exitstatus = code.into_pyobject(py)?.unbind().into_any();
        for func in &hook_funcs {
            python::call_py_hook(
                py,
                func,
                &[
                    ("config", config_proxy.clone_ref(py)),
                    ("session", session_proxy.clone_ref(py)),
                    ("exitstatus", exitstatus.clone_ref(py)),
                ],
            )?;
        }
        Ok(())
    }
}
