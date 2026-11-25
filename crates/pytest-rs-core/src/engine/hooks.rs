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
        // Collection just finished, so per-file collect capture already
        // suspended itself — resume it for the duration of this hook so a
        // conftest's pytest_collection_modifyitems print() lands in this
        // run's own captured buffer instead of leaking into the outer
        // session's (see collect/mod.rs's identical pattern for
        // pytest_cmdline_main). Read the buffered output back while still
        // resumed, but only print it to the real fds *after* restoring
        // capture below — printing while still resumed would just feed
        // straight back into the same capture buffer instead of reaching
        // the terminal — so it appears in real program order (e.g. before
        // --collect-only's own native listing, which prints straight to
        // the fd and would otherwise appear first even though it runs
        // after this hook).
        let was_installed = python::capture_force_resume(py);
        let result = self.fire_collection_modifyitems_inner(py);
        let (out, err) = python::capture_read_now(py);
        python::capture_restore(py, was_installed);
        if !out.is_empty() {
            print!("{out}");
            let _ = std::io::Write::flush(&mut std::io::stdout());
        }
        if !err.is_empty() {
            eprint!("{err}");
        }
        result
    }

    /// Native plugins' `pytest_collection_select_files`: the test files left
    /// to import and collect in this process.
    pub(crate) fn fire_collection_select_files(
        &mut self,
        py: Python<'_>,
        files: &[std::path::PathBuf],
    ) -> PyResult<Vec<std::path::PathBuf>> {
        let mut selected = files.to_vec();
        let mut ctx = HookContext {
            py,
            session: &mut self.session,
            config: &self.config,
        };
        for plugin in self.plugins.iter_mut() {
            plugin.pytest_collection_select_files(&mut ctx, &mut selected)?;
        }
        Ok(selected)
    }

    fn fire_collection_modifyitems_inner(&mut self, py: Python<'_>) -> PyResult<()> {
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
        // Conftest-defined impls only here — third-party plugin modules
        // (e.g. pytest_randomly, pytest-order's OrderingPlugin instance)
        // come exclusively from instance_hook_funcs below. session.py_hooks
        // also holds an entry for entry-point-loaded plugin modules
        // (bootstrap.rs's load_entrypoint_plugins tags them with
        // plugin_module = Some(name)), so including those here too would
        // fire the same third-party hookimpl twice per collection
        // (test_entrypoint_injection: pytest-randomly's module-level
        // pytest_collection_modifyitems reseeding twice instead of once).
        let mut hook_funcs: Vec<Py<pyo3::PyAny>> = self
            .session
            .py_hooks
            .iter()
            .filter(|hook| {
                hook.name == "pytest_collection_modifyitems" && hook.plugin_module.is_none()
            })
            .map(|hook| hook.func.clone_ref(py))
            .collect();
        // Plugins registered at configure time via pluginmanager.register()
        // (e.g. pytest-order's OrderingPlugin) live in pluginmanager._plugins,
        // not session.py_hooks — include their impls (and every entry-point
        // plugin module's) here. instance_hook_impls returns them in plain
        // registration order; stable-sort by tryfirst/normal/trylast tier so
        // e.g. pytest-order's --indulgent-ordering (tryfirst) correctly runs
        // ahead of a normal-priority plugin registered later, matching pluggy.
        let mut instance_funcs = python::instance_hook_funcs(py, "pytest_collection_modifyitems");
        instance_funcs.sort_by_key(|func| Self::modifyitems_hookimpl_tier(py, func));
        // Instance-registered hookwrapper impls (pytest-randomly,
        // pytest-order's --order-after-ff OrderingPlugin) run up to their
        // yield before the plain impls below and resume after the native
        // reorder: see `pending_modifyitems_wrapper_hooks` on Engine and
        // `fire_deferred_modifyitems_wrappers` below. Conftest-level wrapper
        // impls (e.g. a project's own conftest.py) are NOT affected — only
        // plugin-instance-registered ones go through this split.
        let (plain_instance_funcs, mut wrapper_instance_funcs): (Vec<_>, Vec<_>) = instance_funcs
            .into_iter()
            .partition(|func| !Self::modifyitems_hookimpl_is_wrapper(py, func));
        hook_funcs.extend(plain_instance_funcs);
        // pluggy calls tryfirst wrappers first (outermost), trylast ones last.
        wrapper_instance_funcs.sort_by_key(|func| {
            if Self::hookimpl_flag(py, func, "tryfirst") {
                0
            } else if Self::hookimpl_flag(py, func, "trylast") {
                2
            } else {
                1
            }
        });
        self.pending_modifyitems_wrapper_hooks = Vec::new();
        self.pending_modifyitems_list = None;
        let itemcollected_funcs = hook_for("pytest_itemcollected");
        let recording = crate::engine::inprocess::recording();
        if hook_funcs.is_empty()
            && wrapper_instance_funcs.is_empty()
            && itemcollected_funcs.is_empty()
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

        // pytest_itemcollected per item (Django method tags -> marks).
        for func in &itemcollected_funcs {
            for (index, node) in node_list.iter().enumerate() {
                if let Err(err) = python::call_py_hook(py, func, &[("item", node.clone().unbind())])
                {
                    self.session.itemcollected_before_error = Some(index);
                    return Err(err);
                }
            }
        }

        // Plugin wrappers: run each up to its yield, outermost first.
        if !wrapper_instance_funcs.is_empty() {
            let next_fn = py.import("builtins")?.getattr("next")?;
            for func in &wrapper_instance_funcs {
                let wrapper_gen = python::call_py_hook_raw(
                    py,
                    func,
                    &[
                        ("config", config_proxy.clone_ref(py)),
                        ("items", node_list.clone().unbind().into_any()),
                        ("session", python::make_session_proxy(py, &self.config)?),
                    ],
                )?;
                let is_gen: bool = py
                    .import("inspect")?
                    .getattr("isgenerator")?
                    .call1((wrapper_gen.bind(py),))?
                    .extract()?;
                if !is_gen {
                    continue;
                }
                match next_fn.call1((wrapper_gen.bind(py),)) {
                    Ok(_) => {
                        let old_style = Self::hookimpl_flag(py, func, "hookwrapper");
                        self.pending_modifyitems_wrapper_hooks
                            .push((wrapper_gen, old_style));
                    }
                    Err(err) if err.is_instance_of::<pyo3::exceptions::PyStopIteration>(py) => {}
                    Err(err) => return Err(err),
                }
            }
            self.pending_modifyitems_list = Some(node_list.clone().unbind());
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

        // In a nested run, surface per-item itemcollected and the single
        // modifyitems call to the HookRecorder so that
        // getcalls/getreports work as they do in real pytest.
        if recording {
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
            // A hook may substitute this position with a foreign Item object
            // instead of mutating one of the original proxies in place (e.g.
            // pytest-bdd's own robustness test replaces `items` wholesale
            // with an unrelated `pytest.Item` subclass whose own `runtest()`
            // must run). Object identity against the pre-hook `nodes` this
            // engine built distinguishes that from the overwhelmingly common
            // case (reorder/deselect/mark an original proxy in place).
            // Substituted nodes get the same TestItem shape
            // `collect_custom_files` builds for pytest_collect_file-produced
            // custom items, so the existing is_custom_item dispatch in
            // runner/item/setup.rs drives its own runtest() automatically.
            if !nodes.iter().any(|orig| orig.bind(py).is(&node)) {
                let nodeid: String = node.getattr("nodeid")?.extract()?;
                let name: String = node.getattr("name")?.extract()?;
                let path = node
                    .getattr("path")
                    .ok()
                    .and_then(|p| p.str().ok())
                    .map(|s| std::path::PathBuf::from(s.to_string()))
                    .unwrap_or_default();
                let mut marks = Vec::new();
                if let Ok(own) = node.getattr("own_markers") {
                    for mark in own.try_iter()? {
                        let mark = mark?;
                        marks.push(crate::collect::MarkData {
                            name: mark.getattr("name")?.extract()?,
                            obj: mark.unbind(),
                        });
                    }
                }
                items.push(crate::collect::TestItem {
                    nodeid,
                    path,
                    module_name: String::new(),
                    func_name: name,
                    func: node.clone().unbind(),
                    cls: None,
                    is_coroutine: false,
                    is_doctest: false,
                    fixture_names: Vec::new(),
                    extra_fixture_names: Vec::new(),
                    injected_fixture_names: Vec::new(),
                    marks,
                    callspec: Vec::new(),
                    fixture_params: Vec::new(),
                    lineno: 0,
                    collector_class: String::new(),
                    func_class: String::new(),
                    py_node: None,
                    node_class: None,
                    max_param_scope: crate::fixture::Scope::Function,
                    scope_sort_keys: Vec::new(),
                });
                continue;
            }
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
                // Keep the node the hook saw: upstream runs these very
                // objects, so a plugin that holds on to one (to read its
                // `funcargs` or stash later) sees the same item in later
                // hooks; keyword_match_names also reads -k keywords a hook
                // added to it (node.extra_keyword_matches).
                if item.py_node.is_none() {
                    item.py_node = Some(node.clone().unbind());
                }
                // A hook may mutate node.fixturenames in place (e.g.
                // pytest-order's --error-on-failed-ordering pushing a
                // nonexistent name to force a setup error, matching
                // upstream where item.fixturenames itself drives setup).
                // Any name not already known is carried forward so setup
                // attempts to resolve it and errors if unregistered. Custom
                // collector items (make_node exposes the real pytest.Item
                // subclass directly) may have no fixturenames attribute at
                // all — skip those rather than erroring.
                if let Ok(fixturenames) = node.getattr("fixturenames") {
                    let mut known: std::collections::HashSet<String> = item
                        .fixture_names
                        .iter()
                        .chain(item.extra_fixture_names.iter())
                        .chain(item.injected_fixture_names.iter())
                        .cloned()
                        .collect();
                    for name in fixturenames.try_iter()? {
                        let name: String = name?.extract()?;
                        if known.insert(name.clone()) {
                            item.injected_fixture_names.push(name);
                        }
                    }
                }
                items.push(item);
            }
        }
        Ok(())
    }

    /// Reads a boolean flag out of the `pytest_impl` dict the
    /// `pytest.hookimpl(...)` decorator shim attaches to a hook function;
    /// absent means false.
    fn hookimpl_flag(py: Python<'_>, func: &Py<pyo3::PyAny>, flag: &str) -> bool {
        func.bind(py)
            .getattr("pytest_impl")
            .ok()
            .and_then(|d| d.cast_into::<pyo3::types::PyDict>().ok())
            .and_then(|d| d.get_item(flag).ok().flatten())
            .and_then(|v| v.extract::<bool>().ok())
            .unwrap_or(false)
    }

    /// True for a `wrapper=True`/`hookwrapper=True` hookimpl.
    /// Start the `pytest_collection` wrapper impls (conftest and plugin) up
    /// to their yield; `finish_py_collection_wrappers` resumes them. Plain
    /// impls are not called: upstream's is firstresult and a plain impl
    /// returning a value replaces collection, which pytest-rs does not
    /// emulate (same as a plain pytest_runtest_protocol impl).
    pub(crate) fn start_py_collection_wrappers(&mut self, py: Python<'_>) -> PyResult<()> {
        let mut funcs = python::instance_hook_funcs(py, "pytest_collection");
        funcs.extend(
            self.session
                .py_hooks
                .iter()
                .filter(|hook| hook.name == "pytest_collection" && hook.plugin_module.is_none())
                .map(|hook| hook.func.clone_ref(py)),
        );
        // pluggy: the last registered (conftests) is outermost; tryfirst
        // wrappers outermost, trylast innermost.
        funcs.reverse();
        let mut wrappers: Vec<Py<pyo3::PyAny>> = funcs
            .into_iter()
            .filter(|func| Self::modifyitems_hookimpl_is_wrapper(py, func))
            .collect();
        if wrappers.is_empty() {
            return Ok(());
        }
        wrappers.sort_by_key(|func| {
            if Self::hookimpl_flag(py, func, "tryfirst") {
                0
            } else if Self::hookimpl_flag(py, func, "trylast") {
                2
            } else {
                1
            }
        });
        let session = python::make_session_proxy(py, &self.config)?;
        let next_fn = py.import("builtins")?.getattr("next")?;
        let isgenerator = py.import("inspect")?.getattr("isgenerator")?;
        for func in &wrappers {
            let wrapper_gen =
                python::call_py_hook_raw(py, func, &[("session", session.clone_ref(py))])?;
            if !isgenerator
                .call1((wrapper_gen.bind(py),))?
                .extract::<bool>()?
            {
                continue;
            }
            match next_fn.call1((wrapper_gen.bind(py),)) {
                Ok(_) => {
                    let old_style = Self::hookimpl_flag(py, func, "hookwrapper");
                    self.pending_collection_wrappers
                        .push((wrapper_gen, old_style));
                }
                Err(err) if err.is_instance_of::<pyo3::exceptions::PyStopIteration>(py) => {}
                Err(err) => return Err(err),
            }
        }
        Ok(())
    }

    /// The plain (non-wrapper) `pytest_collection` impls of conftests and
    /// plugin modules, called for their side effects. Upstream's hook is
    /// firstresult and an impl returning a value replaces the default
    /// collection; pytest-rs always collects natively, so a returned value is
    /// ignored (like a plain pytest_runtest_protocol impl's).
    pub(crate) fn fire_py_collection_plain(&mut self, py: Python<'_>) -> PyResult<()> {
        let funcs: Vec<Py<pyo3::PyAny>> = self
            .session
            .py_hooks
            .iter()
            .filter(|hook| hook.name == "pytest_collection")
            .map(|hook| hook.func.clone_ref(py))
            .filter(|func| !Self::modifyitems_hookimpl_is_wrapper(py, func))
            .collect();
        if funcs.is_empty() {
            return Ok(());
        }
        let session = python::make_session_proxy(py, &self.config)?;
        // pluggy: the last registered (the conftests) runs first.
        for func in funcs.iter().rev() {
            python::call_py_hook(py, func, &[("session", session.clone_ref(py))])?;
        }
        Ok(())
    }

    /// Resume the suspended `pytest_collection` wrappers (innermost first).
    pub(crate) fn finish_py_collection_wrappers(&mut self, py: Python<'_>) -> PyResult<()> {
        let wrappers = std::mem::take(&mut self.pending_collection_wrappers);
        if wrappers.is_empty() {
            return Ok(());
        }
        crate::runner::finish_runtest_py_wrappers(py, &wrappers, None)
    }

    fn modifyitems_hookimpl_is_wrapper(py: Python<'_>, func: &Py<pyo3::PyAny>) -> bool {
        Self::hookimpl_flag(py, func, "wrapper") || Self::hookimpl_flag(py, func, "hookwrapper")
    }

    /// Sort key for a `pytest_collection_modifyitems` instance hook: 0 for a
    /// plain tryfirst impl, 2 for a plain trylast impl, 1 otherwise (normal
    /// priority, and wrapper/hookwrapper impls left un-hoisted — those are
    /// split out separately, see `modifyitems_hookimpl_is_wrapper`).
    fn modifyitems_hookimpl_tier(py: Python<'_>, func: &Py<pyo3::PyAny>) -> u8 {
        if Self::modifyitems_hookimpl_is_wrapper(py, func) {
            1
        } else if Self::hookimpl_flag(py, func, "tryfirst") {
            0
        } else if Self::hookimpl_flag(py, func, "trylast") {
            2
        } else {
            1
        }
    }

    /// Run any `pytest_collection_modifyitems` hookwrapper impls that were
    /// deferred by `run_py_modifyitems` (instance-registered wrappers only,
    /// e.g. pytest-order's `--order-after-ff` OrderingPlugin). Called after
    /// the native --lf/--ff/--nf/--sw reorder (`cache.modify_items`) so these
    /// wrappers' post-yield mutation sees the settled item list — matching
    /// upstream pluggy semantics where LFPlugin's own hookwrapper reorder
    /// (also entirely post-yield) is innermost and any outer wrapper's
    /// post-yield code runs after it. See
    /// pytest_order_ff_reorder_hookwrapper_ordering_gap in project memory for
    /// the full analysis.
    pub(crate) fn fire_deferred_modifyitems_wrappers(&mut self, py: Python<'_>) -> PyResult<()> {
        let gens = std::mem::take(&mut self.pending_modifyitems_wrapper_hooks);
        let held_list = self.pending_modifyitems_list.take();
        let Some(held_list) = held_list.filter(|_| !gens.is_empty()) else {
            return Ok(());
        };
        let mut items = std::mem::take(&mut self.session.items);
        let nodes: Vec<Py<pyo3::PyAny>> = items
            .iter()
            .map(|item| python::make_node(py, item))
            .collect::<PyResult<_>>()?;
        // The wrappers hold the list they were called with: refill it in
        // place with the settled items, as upstream mutates one list.
        let node_list = held_list.bind(py).clone();
        let fresh = pyo3::types::PyList::new(py, nodes.iter().map(|n| n.bind(py)))?;
        let all = pyo3::types::PySlice::full(py);
        node_list.as_any().set_item(all, fresh)?;
        // Resume innermost first.
        let result_cls = py.import("pytest._pluginmanager")?.getattr("_Result")?;
        for (wrapper_gen, old_style) in gens.iter().rev() {
            let sent = if *old_style {
                result_cls.call1((py.None(),))?.unbind()
            } else {
                py.None()
            };
            match wrapper_gen.bind(py).call_method1("send", (sent,)) {
                Ok(_) => {
                    self.session.items = items;
                    return Err(pyo3::exceptions::PyRuntimeError::new_err(
                        "pytest_collection_modifyitems wrapper yielded more than once",
                    ));
                }
                Err(err) if err.is_instance_of::<pyo3::exceptions::PyStopIteration>(py) => {}
                Err(err) => {
                    self.session.items = items;
                    return Err(err);
                }
            }
        }
        // Read back order/membership (by nodeid) and any added markers —
        // same reconciliation as run_py_modifyitems.
        let mut by_nodeid: std::collections::HashMap<
            String,
            std::collections::VecDeque<crate::collect::TestItem>,
        > = Default::default();
        for item in items.drain(..) {
            by_nodeid
                .entry(item.nodeid.clone())
                .or_default()
                .push_back(item);
        }
        for node in node_list.iter() {
            // Same foreign-substitution handling as run_py_modifyitems (see
            // its comment) — object identity against the pre-hook `nodes`
            // distinguishes a hook-substituted Item from an original proxy
            // mutated in place.
            if !nodes.iter().any(|orig| orig.bind(py).is(&node)) {
                let nodeid: String = node.getattr("nodeid")?.extract()?;
                let name: String = node.getattr("name")?.extract()?;
                let path = node
                    .getattr("path")
                    .ok()
                    .and_then(|p| p.str().ok())
                    .map(|s| std::path::PathBuf::from(s.to_string()))
                    .unwrap_or_default();
                let mut marks = Vec::new();
                if let Ok(own) = node.getattr("own_markers") {
                    for mark in own.try_iter()? {
                        let mark = mark?;
                        marks.push(crate::collect::MarkData {
                            name: mark.getattr("name")?.extract()?,
                            obj: mark.unbind(),
                        });
                    }
                }
                items.push(crate::collect::TestItem {
                    nodeid,
                    path,
                    module_name: String::new(),
                    func_name: name,
                    func: node.clone().unbind(),
                    cls: None,
                    is_coroutine: false,
                    is_doctest: false,
                    fixture_names: Vec::new(),
                    extra_fixture_names: Vec::new(),
                    injected_fixture_names: Vec::new(),
                    marks,
                    callspec: Vec::new(),
                    fixture_params: Vec::new(),
                    lineno: 0,
                    collector_class: String::new(),
                    func_class: String::new(),
                    py_node: None,
                    node_class: None,
                    max_param_scope: crate::fixture::Scope::Function,
                    scope_sort_keys: Vec::new(),
                });
                continue;
            }
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
        self.session.items = items;
        Ok(())
    }

    /// pytest_collectstart / pytest_make_collect_report /
    /// pytest_collectreport for every collector of the native collection,
    /// replayed depth-first in upstream's order by
    /// `pytest._collectreport.CollectReportDispatch` through the shim
    /// pluginmanager — the one path conftests, plugins, a replacement
    /// reporter and a pytester HookRecorder all receive them from. Runs once
    /// collection settled and before collection errors are handled, so a
    /// failed or skipped file's report is delivered even when the errors
    /// abort the session. `errors` are the collection errors as (nodeid,
    /// longrepr). Markers hooks added to collectors (pytest-django's Class
    /// tags in pytest_collectstart) are added to the items below them.
    /// Nothing is built unless something would receive a call.
    pub(crate) fn dispatch_collect_reports(
        &mut self,
        py: Python<'_>,
        errors: &[(String, String)],
    ) -> PyResult<()> {
        use pyo3::types::PyList;
        let dispatcher = py
            .import("pytest._collectreport")?
            .getattr("CollectReportDispatch")?;
        if !dispatcher.call_method0("wanted")?.extract::<bool>()? {
            return Ok(());
        }
        let config = python::make_py_config(py, &self.config)?;
        let items = PyList::empty(py);
        for item in &self.session.items {
            let kind = if python::is_custom_item(py, &item.func) {
                "custom"
            } else if item.is_doctest {
                "doctest"
            } else {
                ""
            };
            items.append((item.nodeid.as_str(), python::make_node(py, item)?, kind))?;
        }
        let skipped_dirs: Vec<&str> = self
            .session
            .skipped_conftest_dirs
            .iter()
            .map(|(_, nodeid, ..)| nodeid.as_str())
            .collect();
        let skips: Vec<(&str, &str, &str, bool)> = self
            .session
            .skipped_modules
            .iter()
            .map(|(nodeid, reason, location)| {
                (
                    nodeid.as_str(),
                    reason.as_str(),
                    location.as_str(),
                    skipped_dirs.contains(&nodeid.as_str()),
                )
            })
            .collect();
        let added_marks = dispatcher.call_method1(
            "dispatch",
            (
                config,
                self.config.rootdir.to_string_lossy().as_ref(),
                self.collect_arg_nodeids(),
                items,
                errors.to_vec(),
                skips,
                self.session.doctest_collect_skips.clone(),
                self.session.collect_file_skips.clone(),
            ),
        )?;
        for entry in added_marks.try_iter()? {
            let (index, marks): (usize, Vec<Bound<'_, PyAny>>) = entry?.extract()?;
            let Some(item) = self.session.items.get_mut(index) else {
                continue;
            };
            for mark in marks {
                item.marks.push(crate::collect::MarkData {
                    name: mark.getattr("name")?.extract()?,
                    obj: mark.unbind(),
                });
            }
        }
        Ok(())
    }

    /// The node ids the command-line arguments name ("." for the rootdir),
    /// whose nodes are the Session collectreport's result. Empty (the
    /// rootdir is the match) without arguments or when one is not a path
    /// (`--pyargs`).
    fn collect_arg_nodeids(&self) -> Vec<String> {
        let rootdir = crate::collect::canonicalize_preserving_symlinks(&self.config.rootdir);
        let mut out = Vec::new();
        for arg in &self.config.paths {
            let (path_arg, names) = match arg.split_once("::") {
                Some((path, names)) => (path, Some(names)),
                None => (arg.as_str(), None),
            };
            let path = self.config.invocation_dir.join(path_arg);
            if !path.exists() {
                return Vec::new();
            }
            let path = crate::collect::canonicalize_preserving_symlinks(&path);
            let mut nodeid =
                crate::collect::file_nodeid(&rootdir, &path, &self.session.initial_paths);
            if nodeid.is_empty() {
                nodeid = ".".to_string();
            }
            if let Some(names) = names {
                nodeid = format!("{nodeid}::{names}");
            }
            out.push(nodeid);
        }
        out
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
        // Unlike the other hook names with a session.py_hooks/instance_hook_funcs
        // split (e.g. pytest_collection_modifyitems), this one is NOT filtered
        // to conftest-only: bootstrap.rs's load_named_plugins (a plain `-p
        // NAME` module import, as opposed to its own entry-point sub-branch or
        // load_entrypoint_plugins) never registers the module with
        // pluginmanager, only scans its hooks into session.py_hooks — so
        // instance_hook_funcs can't see it, and excluding plugin_module-tagged
        // entries here would silently drop its pytest_addoption entirely
        // (regressed test_config_preparse_plugin_option: `-p pytest_xyz
        // --xyz=123` needs pytest_xyz's addoption to run so --xyz is a known
        // flag). Calling addoption twice for a plugin that happens to be both
        // module-scanned and instance-registered is harmless — addoption is a
        // dict overwrite (OptionGroup.addoption in _parser.py), not a
        // side-effecting action like a reseed callback.
        let mut hook_funcs: Vec<Py<pyo3::PyAny>> = self
            .session
            .py_hooks
            .iter()
            .filter(|hook| hook.name == "pytest_addoption")
            .map(|hook| hook.func.clone_ref(py))
            .collect();
        // Plugin INSTANCES registered directly (e.g. pytest.main(...,
        // plugins=[MyPlugin()]) or Pytester's helper_plugin) never go through
        // session.py_hooks' bootstrap-time module scan at all.
        hook_funcs.extend(python::instance_hook_funcs(py, "pytest_addoption"));
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
        // -h/--help: Config::help_text (config/cli.rs) is rendered once, at
        // initial CLI parsing, from only the native Rust plugins' options —
        // long before conftest/plugin pytest_addoption hooks (this function)
        // ever run, so it can't reflect their flags on its own. `pytest._parser`
        // tracks every addoption() call in its module-level `flag_dests` dict,
        // but that dict is a process-wide accumulator never cleared between
        // nested runs (matching every other pytest._parser registry) — so
        // diff its keys immediately before/after firing THIS run's own
        // hookimpls, rather than dumping the whole (potentially
        // cross-run-polluted) dict, and append just those as a plain listing.
        let flag_dest_keys = |py: Python<'_>| -> std::collections::HashSet<String> {
            let Ok(dict) = py
                .import("pytest._parser")
                .and_then(|m| m.getattr("flag_dests"))
            else {
                return Default::default();
            };
            let Ok(keys) = dict.call_method0("keys") else {
                return Default::default();
            };
            let Ok(iter) = keys.try_iter() else {
                return Default::default();
            };
            iter.filter_map(|k| k.ok()?.extract::<String>().ok())
                .collect()
        };
        let before_flags = if self.config.help_text.is_some() {
            flag_dest_keys(py)
        } else {
            Default::default()
        };
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
        if self.config.help_text.is_some() {
            let after_flags = flag_dest_keys(py);
            let mut new_flags: Vec<String> =
                after_flags.difference(&before_flags).cloned().collect();
            new_flags.sort();
            if !new_flags.is_empty() && self.config.help_text.is_some() {
                // Each flag renders under its own parser.getgroup("name")
                // heading (matching upstream's per-plugin option groups) via
                // argparse's own HelpFormatter, so wrapping/alignment
                // matches upstream's argparse-rendered --help exactly; a
                // bare parser.addoption() call falls into "custom options:".
                if let Ok(rendered) = py
                    .import("pytest._parser")
                    .and_then(|m| m.call_method1("render_new_option_help", (new_flags,)))
                    .and_then(|r| r.extract::<String>())
                    && !rendered.is_empty()
                {
                    self.config.plugin_option_help.push('\n');
                    self.config.plugin_option_help.push_str(&rendered);
                }
            }
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
        // -h/--help is a recognized clap flag (not an "unknown" plugin arg), so
        // it never lands in plugin_args — re-add it here or a plugin's own
        // `if options.help: return` early-exit (e.g. pytest-django's Django
        // project scan) never sees it and runs unwanted setup during --help.
        // `-VV` likewise (pytest-django sets Django up for it too).
        let version_args: &[&str] = if self.config.version_text.is_some() {
            &["--version", "--version"]
        } else {
            &[]
        };
        let full_args: Vec<&str> = self
            .config
            .help_text
            .is_some()
            .then_some("--help")
            .into_iter()
            .chain(version_args.iter().copied())
            .chain(self.config.plugin_args.iter().map(String::as_str))
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
    pub(crate) fn apply_plugin_cli_args(
        &mut self,
        py: Python<'_>,
        raise_on_unknown: bool,
    ) -> PyResult<()> {
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
        if !raise_on_unknown {
            // Early pass (Engine::collect, before conftest loading): specs
            // for conftest-defined options don't exist yet, so a token that's
            // "unknown" here may just not have been registered yet — only
            // the later, full pass (after every conftest has loaded) can
            // tell a truly-unrecognized flag from one it just hasn't reached
            // yet.
            return Ok(());
        }
        // A genuinely unrecognized flag is only found by scanning every
        // token to the end (upstream: argparse's parse_known_args collects
        // it into "extras" and keeps scanning). `-h`/`--help`'s own argparse
        // Action interrupts parsing the instant it's reached — via a raised
        // PrintHelp, before parse_args ever gets back around to checking
        // those extras — so an unrelated unknown flag earlier in the same
        // addopts/CLI never becomes a UsageError when --help is present.
        // A malformed *registered* option (below, via a raised UsageError
        // from apply_cli_args) still fails immediately regardless: argparse
        // raises synchronously the moment it tries and fails to consume that
        // option's own value, before ever reaching --help's token.
        if !unknown.is_empty() && self.config.help_text.is_none() {
            // Match argparse/pytest's MyOptionParser.error: a
            // "<prog>: error: <message>" line followed by the sorted
            // extra_info (inifile, rootdir) pytest attaches to the parser.
            let mut msg = format!(
                "{}: error: unrecognized arguments: {}",
                crate::config::Config::prog_name(),
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

    /// Only ever called for "pytest_unconfigure" (every call site in
    /// session.rs passes that literal name) — also drains `config`'s
    /// `add_cleanup` stack afterward (upstream's `Config._ensure_unconfigure`
    /// fires `pytest_unconfigure` then drains `_cleanup_stack`; pytest-rs
    /// splits the same two steps across this function and
    /// `PyConfig::_ensure_unconfigure`), so `config.add_cleanup(...)`
    /// registrations (e.g. `_configure_python_path`'s sys.path cleanup, or a
    /// conftest/fixture's own `request.config.add_cleanup`) actually run in a
    /// real session — previously only reachable from the `parseconfig`-style
    /// standalone Config paths.
    pub(crate) fn fire_py_hooks_simple(&mut self, py: Python<'_>, name: &str) -> PyResult<()> {
        self.fire_py_unconfigure_impls(py, name)?;
        self.drain_config_cleanups(py)
    }

    /// The `pytest_unconfigure` impls alone (see `fire_py_hooks_simple`), for
    /// the session end, which runs upstream's own unraisableexception
    /// `pytest_unconfigure` between them and the cleanup-stack drain.
    pub(crate) fn fire_py_unconfigure_impls(&mut self, py: Python<'_>, name: &str) -> PyResult<()> {
        // Respect @pytest.hookimpl(tryfirst/trylast) ordering: tryfirst before
        // normal, normal next, trylast last (mirrors pluggy's call ordering).
        // Conftest-defined impls only from py_hooks — third-party plugin
        // modules (entry-point/`-p` loaded) come exclusively from
        // instance_hook_funcs below; session.py_hooks also carries an entry
        // for those (bootstrap.rs tags it with plugin_module), so including
        // both would fire the same third-party hookimpl twice (same
        // precedent as run_py_modifyitems above).
        let mut hooks: Vec<_> = self
            .session
            .py_hooks
            .iter()
            .filter(|hook| hook.name == name && hook.plugin_module.is_none())
            .collect();
        hooks.sort_by_key(|h| match (h.tryfirst, h.trylast) {
            (true, _) => 0,
            (_, true) => 2,
            _ => 1,
        });
        let mut hook_funcs: Vec<Py<pyo3::PyAny>> =
            hooks.iter().map(|h| h.func.clone_ref(py)).collect();
        // Plugin instances registered via pluginmanager.register() (e.g.
        // pytest.main(..., plugins=[MyPlugin()])) never go through
        // session.py_hooks' bootstrap-time module scan at all.
        hook_funcs.extend(python::instance_hook_funcs(py, name));
        // pluggy fires the HookCaller even with zero implementations, so an
        // in-process HookRecorder records the (empty) call regardless. Skip
        // only the hook-firing loop when there's nothing to call and no
        // recorder to notify — the cleanup-stack drain below still needs to
        // run unconditionally (pythonpath cleanup has no hookimpl to trigger
        // it).
        let skip_hooks = hook_funcs.is_empty() && !crate::engine::inprocess::recording();
        let config_proxy = python::make_py_config(py, &self.config)?;
        if !skip_hooks {
            // Record the hook call so an in-process HookRecorder's getcalls
            // sees it (the native engine dispatches conftest/plugin hooks
            // directly, bypassing pluggy's HookCaller that the monitoring
            // wraps).
            python::record_hook(py, name, &[("config", config_proxy.clone_ref(py))]);
            for func in &hook_funcs {
                python::call_py_hook(py, func, &[("config", config_proxy.clone_ref(py))])?;
            }
        }
        Ok(())
    }

    /// Upstream's `_cleanup_stack.close()` after `pytest_unconfigure`: the
    /// pythonpath cleanup and the `config.add_cleanup` callbacks.
    pub(crate) fn drain_config_cleanups(&mut self, py: Python<'_>) -> PyResult<()> {
        let config_proxy = python::make_py_config(py, &self.config)?;
        python::pythonpath_cleanup(py, &self.config);
        let _ = config_proxy.bind(py).call_method0("_ensure_unconfigure");
        Ok(())
    }

    /// pytest_configure fires in two passes, with the default
    /// 'terminalreporter' plugin registering between them. Upstream
    /// registers built-in plugins (incl. `_pytest.terminal`, which creates
    /// `terminalreporter`) before `-p`/entry-point plugins, so pluggy's LIFO
    /// call order runs a `-p` plugin's normal-priority `pytest_configure`
    /// *before* `_pytest.terminal`'s — hence `terminalreporter` doesn't exist
    /// yet from a plain `-p` plugin's point of view. Reporter-replacing
    /// plugins (pytest-sugar/pytest-pretty) mark their own `pytest_configure`
    /// `trylast=True` specifically so it runs *after* `_pytest.terminal`'s,
    /// once `terminalreporter` exists and can be swapped out.
    pub(crate) fn fire_py_configure(
        &mut self,
        py: Python<'_>,
        rootdir: &std::path::Path,
        errors: &mut Vec<(std::path::PathBuf, String)>,
    ) -> PyResult<()> {
        // pytest 9.2's builtin plugins read these Literal-typed inis in their
        // own pytest_configure, so an invalid value is a usage error at
        // startup (terminal, mark, logging; junitxml only with --junitxml).
        {
            let config_proxy = python::make_py_config(py, &self.config)?;
            let mut names = vec![
                "console_output_style",
                "empty_parameter_set_mark",
                "tmp_path_retention_count",
                "tmp_path_retention_policy",
            ];
            if self.config.get_value("log-file-mode").is_none() {
                names.push("log_file_mode");
            }
            if self.config.get_value("junit-xml").is_some() {
                names.extend(["junit_logging", "junit_duration_report", "junit_family"]);
            }
            for ini in names {
                config_proxy.call_method1(py, "getini", (ini,))?;
            }
        }
        let name = "pytest_configure";
        let mut hooks: Vec<_> = self
            .session
            .py_hooks
            .iter()
            .filter(|hook| hook.name == name)
            .collect();
        // pluggy LIFO: within one priority tier, the LAST-registered hookimpl
        // fires FIRST. session.py_hooks accumulates in bootstrap order
        // (native/-p/entry-point plugins, then conftest.py files, loaded
        // later) — reverse that scan order before the tier sort so its own
        // stability preserves LIFO within each tier (e.g. a conftest's
        // normal-priority pytest_configure, registered after a `-p` plugin's,
        // now correctly fires first — upstream's tests/test_initialization.py
        // ::test_django_setup_order_and_uniqueness pins this exact ordering).
        hooks.reverse();
        hooks.sort_by_key(|h| match (h.tryfirst, h.trylast) {
            (true, _) => 0,
            (_, true) => 2,
            _ => 1,
        });
        let before_funcs: Vec<Py<pyo3::PyAny>> = hooks
            .iter()
            .filter(|h| !h.trylast)
            .map(|h| h.func.clone_ref(py))
            .collect();
        let after_funcs: Vec<Py<pyo3::PyAny>> = hooks
            .iter()
            .filter(|h| h.trylast)
            .map(|h| h.func.clone_ref(py))
            .collect();

        let mut before_result = Ok(());
        if !before_funcs.is_empty()
            || !after_funcs.is_empty()
            || crate::engine::inprocess::recording()
        {
            let config_proxy = python::make_py_config(py, &self.config)?;
            python::record_hook(py, name, &[("config", config_proxy.clone_ref(py))]);
            let _ = python::set_reporter_configuring(py, true);
            for func in &before_funcs {
                if let Err(err) =
                    python::call_py_hook(py, func, &[("config", config_proxy.clone_ref(py))])
                {
                    before_result = Err(err);
                    break;
                }
            }
            let _ = python::set_reporter_configuring(py, false);
        }
        if let Err(err) = python::reporter_setup(py, &self.config) {
            errors.push((rootdir.to_path_buf(), python::format_exception(py, &err)));
        }
        before_result?;
        if !after_funcs.is_empty() {
            let config_proxy = python::make_py_config(py, &self.config)?;
            for func in &after_funcs {
                python::call_py_hook(py, func, &[("config", config_proxy.clone_ref(py))])?;
            }
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
    /// A conftest's own plain (non-hookwrapper) `pytest_runtestloop` hookimpl
    /// — firstresult, like `pytest_runtest_protocol`'s `delegate_protocol` —
    /// may replace the native item-running loop entirely (returning
    /// non-`None`) or just observe/log and fall through (`None`). Unlike
    /// `delegate_protocol`, this doesn't support a *replacing* impl driving
    /// items itself re-entrantly (no pinned conformance test needs that);
    /// it only supports skipping pytest-rs's own native loop when one
    /// returns non-`None`. Returns whether the loop was replaced.
    pub(crate) fn fire_py_runtestloop(&mut self, py: Python<'_>) -> PyResult<bool> {
        let isgenfunc = py.import("inspect")?.getattr("isgeneratorfunction")?;
        let hook_funcs: Vec<Py<pyo3::PyAny>> = self
            .session
            .py_hooks
            .iter()
            .filter(|hook| hook.name == "pytest_runtestloop")
            .map(|hook| hook.func.clone_ref(py))
            .filter(|func| {
                !isgenfunc
                    .call1((func.bind(py),))
                    .and_then(|v| v.extract::<bool>())
                    .unwrap_or(false)
            })
            .collect();
        if hook_funcs.is_empty() {
            return Ok(false);
        }
        let session = python::make_session_proxy(py, &self.config)?;
        for func in &hook_funcs {
            let result = python::call_py_hook(py, func, &[("session", session.clone_ref(py))])?;
            if !result.bind(py).is_none() {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Validate every scanned conftest/plugin hookimpl name against known
    /// hookspecs (upstream's `check_pending()`, called right after
    /// collection, before `pytest_collection_modifyitems`): a `pytest_*`
    /// name that isn't a real hookspec (a typo, e.g. `pytest_hello`) is an
    /// error unless marked `optionalhook`.
    pub(crate) fn check_pending_hooks(&mut self, py: Python<'_>) -> PyResult<()> {
        let entries = pyo3::types::PyList::empty(py);
        for hook in &self.session.py_hooks {
            // @pytest.hookimpl(specname="pytest_x") lets a hookimpl function
            // be named anything (e.g. pytest-asyncio's
            // pytest_pycollect_makeitem_convert_async_functions_to_subclass,
            // implementing pytest_pycollect_makeitem) — validate against
            // that declared name, not the function's own literal name.
            let effective_name = hook
                .func
                .bind(py)
                .getattr("pytest_impl")
                .ok()
                .and_then(|d| d.get_item("specname").ok())
                .and_then(|v| v.extract::<String>().ok())
                .unwrap_or_else(|| hook.name.clone());
            entries.append((effective_name, hook.optionalhook, hook.baseid.as_str()))?;
        }
        py.import("pytest._pluginmanager")?
            .getattr("pluginmanager")?
            .call_method1("check_pending_hooks", (entries,))?;
        Ok(())
    }

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
        // Conftest-defined impls only here — third-party plugin modules
        // (e.g. anyio's own pytest_plugin) are already covered by
        // instance_hook_funcs above; session.py_hooks also carries an entry
        // for entry-point-loaded plugin modules (plugin_module = Some(name)),
        // so including those here too would fire the same hookimpl twice.
        hook_funcs.extend(
            self.session
                .py_hooks
                .iter()
                .filter(|hook| {
                    hook.name == "pytest_collection_finish" && hook.plugin_module.is_none()
                })
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
        // Collection ended early (errors, nothing collected): pytest_collection
        // wrappers still suspended resume before the session finishes.
        if let Err(err) = self.finish_py_collection_wrappers(py) {
            eprintln!("INTERNAL ERROR: {}", python::format_exception(py, &err));
        }
        let mut hook_funcs = python::instance_hook_funcs(py, "pytest_sessionfinish");
        // Conftest-defined impls only here — third-party plugin modules
        // (e.g. pytest-ruff) are already covered by instance_hook_funcs
        // above; session.py_hooks also carries an entry for entry-point-
        // loaded plugin modules (plugin_module = Some(name)), so including
        // those here too would fire the same hookimpl twice.
        hook_funcs.extend(
            self.session
                .py_hooks
                .iter()
                .filter(|hook| hook.name == "pytest_sessionfinish" && hook.plugin_module.is_none())
                .map(|hook| hook.func.clone_ref(py)),
        );
        if hook_funcs.is_empty() {
            return self.tmp_path_session_finish(py, code);
        }
        let config_proxy = python::make_py_config(py, &self.config)?;
        let session_proxy = python::make_session_proxy(py, &self.config)?;
        let exitstatus = code.into_pyobject(py)?.unbind().into_any();
        // Upstream returns `session.exitstatus` after this hook, so a plugin
        // may override the exit code there (pytest-rerunfailures'
        // --fail-on-flaky sets 7).
        let _ = session_proxy.bind(py).setattr("exitstatus", code);
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
        if let Ok(updated) = session_proxy
            .bind(py)
            .getattr("exitstatus")
            .and_then(|v| v.extract::<i32>())
            && updated != code
        {
            self.session.exit_code_override = Some(updated);
        }
        self.tmp_path_session_finish(py, code)
    }

    /// The tmpdir plugin's own pytest_sessionfinish (registered before
    /// conftests, so it runs after their impls): the "failed" retention
    /// policy's basetemp removal and the numbered-dir cleanups. Runs outside
    /// any test, so its warnings are recorded with when="config", like
    /// upstream's warnings-plugin wrapper around pytest_sessionfinish.
    fn tmp_path_session_finish(&self, py: Python<'_>, code: i32) -> PyResult<()> {
        let config_proxy = python::make_py_config(py, &self.config)?;
        let wcapture = py.import("pytest._wcapture")?;
        wcapture.call_method1("set_current_test", (py.None(),))?;
        wcapture.call_method1("set_current_when", ("config",))?;
        py.import("pytest._tmp_path")?
            .call_method1("session_finish", (config_proxy, code))?;
        Ok(())
    }
}
