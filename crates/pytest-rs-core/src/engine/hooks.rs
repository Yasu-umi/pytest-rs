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
        hook_funcs.extend(python::instance_hook_funcs(py, "pytest_collection_modifyitems"));
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

        // pytest_collectstart per distinct test class: build a pytest.Class
        // collector (.obj = the class), fire the hooks, and propagate any
        // markers it added to the class's item nodes (Django tags -> marks).
        if !collectstart_funcs.is_empty() {
            let class_cls = py.import("pytest._node")?.getattr("Class")?;
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

    /// Emit pytest_collectstart + pytest_collectreport pairs for the full
    /// collector tree to the in-process HookRecorder.  Called only when
    /// recording() is true (inside a nested run).
    ///
    /// pytest collects in a tree (Session → Dir/Package → Module → Class →
    /// Function) and fires these hooks as each collector opens and closes.
    /// We reconstruct that tree from the flat item list and the collect-error
    /// list that were already captured by the time modifyitems runs.
    ///
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

        // Helper: emit pytest_collectstart with a stub collector (nodeid only).
        let emit_start = |nodeid: &str| {
            let kw = PyDict::new(py);
            let _ = kw.set_item("nodeid", nodeid);
            if let Ok(stub) = simple_ns.call((), Some(&kw)) {
                python::record_hook(py, "pytest_collectstart", &[("collector", stub.unbind())]);
            }
        };

        // Helper: emit pytest_collectreport(passed).
        let emit_passed = |nodeid: &str| -> PyResult<()> {
            let kw = PyDict::new(py);
            kw.set_item("nodeid", nodeid)?;
            kw.set_item("outcome", "passed")?;
            kw.set_item("longrepr", py.None())?;
            let file = nodeid.split("::").next().unwrap_or(nodeid);
            kw.set_item("location", (file, py.None(), file))?;
            kw.set_item("result", PyList::empty(py))?;
            kw.set_item("sections", PyList::empty(py))?;
            let report = collect_report_cls.call((), Some(&kw))?.unbind();
            python::record_hook(py, "pytest_collectreport", &[("report", report)]);
            Ok(())
        };

        // Helper: emit pytest_collectreport(skipped) for module-level skips.
        // longrepr is a (file, line, "Skipped: reason") tuple as pytest emits.
        let emit_skipped = |nodeid: &str, reason: &str, location: &str| -> PyResult<()> {
            // Parse "file:line" location into file and lineno.
            let (loc_file, lineno) = if let Some(colon) = location.rfind(':') {
                let f = &location[..colon];
                let ln: u64 = location[colon + 1..].parse().unwrap_or(1);
                (f, ln)
            } else {
                (location, 1u64)
            };
            let skip_reason = format!("Skipped: {reason}");
            let longrepr = (loc_file, lineno, skip_reason);
            let kw = PyDict::new(py);
            kw.set_item("nodeid", nodeid)?;
            kw.set_item("outcome", "skipped")?;
            kw.set_item("longrepr", longrepr)?;
            let file = nodeid.split("::").next().unwrap_or(nodeid);
            kw.set_item("location", (file, py.None(), nodeid))?;
            kw.set_item("result", PyList::empty(py))?;
            kw.set_item("sections", PyList::empty(py))?;
            let report = collect_report_cls.call((), Some(&kw))?.unbind();
            python::record_hook(py, "pytest_collectreport", &[("report", report)]);
            Ok(())
        };

        // Unique passing-module paths (nodeid prefix before first "::").
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

        // Unique failing-module nodeids from session.collect_errors
        // (their collectreport was already emitted by reporter_collect_error).
        let failing_modules: Vec<&str> = self
            .session
            .collect_errors
            .iter()
            .map(|(nodeid, _)| nodeid.as_str())
            .collect();

        // Unique skipped-module nodeids (pytest.skip(allow_module_level=True), etc.).
        let skipped_modules: Vec<(&str, &str, &str)> = self
            .session
            .skipped_modules
            .iter()
            .map(|(nodeid, reason, loc)| (nodeid.as_str(), reason.as_str(), loc.as_str()))
            .collect();

        // Unique directories (parent of each module file; "" → ".").
        let mut dirs: Vec<String> = Vec::new();
        {
            let mut seen: std::collections::HashSet<String> = Default::default();
            let all_files = passing_modules
                .iter()
                .map(|s| s.as_str())
                .chain(failing_modules.iter().copied())
                .chain(skipped_modules.iter().map(|(nodeid, _, _)| *nodeid));
            for file in all_files {
                let dir = match std::path::Path::new(file).parent() {
                    Some(p) if p.as_os_str().is_empty() => ".".to_string(),
                    Some(p) => p.to_string_lossy().into_owned(),
                    None => ".".to_string(),
                };
                if seen.insert(dir.clone()) {
                    dirs.push(dir);
                }
            }
        }

        // Unique class nodeids ("file::ClassName") for items inside a class.
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

        // ── Emit collector tree ──────────────────────────────────────────────
        // Real pytest order (perform_collect + genitems post-order):
        //   collectstarts: Session → Dirs → Modules (top-down)
        //   collectreports: Session first (before genitems), then per genitems
        //     post-order: Class reports → Module reports → Dir reports
        // Failing-module collectreports are already recorded; we only start them.

        // collectstarts (top-down: Session → Dirs → Modules)
        emit_start("");
        for dir in &dirs {
            emit_start(dir.as_str());
        }
        for file in &passing_modules {
            emit_start(file.as_str());
        }
        for (nodeid, _, _) in &skipped_modules {
            emit_start(nodeid);
        }
        for nodeid in &failing_modules {
            emit_start(nodeid);
        }

        // collectreports (Session first, then post-order: Class → Module → Dir)
        emit_passed("")?; // Session

        for class in &classes {
            emit_start(class.as_str());
            emit_passed(class.as_str())?;
        }
        for file in &passing_modules {
            emit_passed(file.as_str())?;
        }
        for (nodeid, reason, location) in &skipped_modules {
            emit_skipped(nodeid, reason, location)?;
        }
        for dir in &dirs {
            emit_passed(dir.as_str())?;
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
        let hook_funcs: Vec<Py<pyo3::PyAny>> = self
            .session
            .py_hooks
            .iter()
            .filter(|hook| hook.name == "pytest_load_initial_conftests")
            .map(|hook| hook.func.clone_ref(py))
            .collect();
        if hook_funcs.is_empty() {
            return Ok(());
        }
        let early_config = python::make_py_config(py, &self.config)?;
        let parser = py.import("pytest._parser")?.getattr("parser")?.unbind();
        let args = pyo3::types::PyList::new(py, &self.config.paths)?
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
        let hook_funcs: Vec<Py<pyo3::PyAny>> = self
            .session
            .py_hooks
            .iter()
            .filter(|hook| hook.name == name)
            .map(|hook| hook.func.clone_ref(py))
            .collect();
        if hook_funcs.is_empty() {
            return Ok(());
        }
        let config_proxy = python::make_py_config(py, &self.config)?;
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
