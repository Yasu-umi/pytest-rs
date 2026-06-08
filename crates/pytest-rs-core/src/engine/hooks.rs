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
        let hook_funcs: Vec<Py<pyo3::PyAny>> = self
            .session
            .py_hooks
            .iter()
            .filter(|hook| hook.name == "pytest_collection_modifyitems")
            .map(|hook| hook.func.clone_ref(py))
            .collect();
        if hook_funcs.is_empty() {
            return Ok(());
        }

        let config_proxy = python::make_py_config(py, &self.config)?;
        let nodes: Vec<Py<pyo3::PyAny>> = items
            .iter()
            .map(|item| python::make_node(py, item))
            .collect::<PyResult<_>>()?;
        let node_list = pyo3::types::PyList::new(py, nodes.iter().map(|n| n.bind(py)))?;

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

        // Read back order/membership (by nodeid) and any added markers.
        let mut by_nodeid: std::collections::HashMap<String, crate::collect::TestItem> =
            std::mem::take(items)
                .into_iter()
                .map(|item| (item.nodeid.clone(), item))
                .collect();
        for node in node_list.iter() {
            let nodeid: String = node.getattr("nodeid")?.extract()?;
            if let Some(mut item) = by_nodeid.remove(&nodeid) {
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

    /// pytest_deselected conftest/plugin hooks: called once with every item
    /// dropped by -k/-m/--lf selection (a copy, like pytest's list).
    pub(crate) fn fire_py_deselected(&mut self, py: Python<'_>) -> PyResult<()> {
        if self.session.deselected_items.is_empty() {
            return Ok(());
        }
        let hook_funcs: Vec<Py<pyo3::PyAny>> = self
            .session
            .py_hooks
            .iter()
            .filter(|hook| hook.name == "pytest_deselected")
            .map(|hook| hook.func.clone_ref(py))
            .collect();
        let delegated = self.session.custom_reporter.is_some();
        if hook_funcs.is_empty() && !delegated {
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
        let unknown: Vec<String> = py
            .import("pytest._parser")?
            .call_method1("apply_cli_args", (option, self.config.plugin_args.clone()))?
            .extract()?;
        if !unknown.is_empty() {
            // Match argparse/pytest's MyOptionParser.error: a
            // "<prog>: error: <message>" line followed by the sorted
            // extra_info (inifile, rootdir) pytest attaches to the parser.
            let mut msg = format!("pytest: error: unrecognized arguments: {}", unknown.join(" "));
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
        if hook_funcs.is_empty() && self.session.custom_reporter.is_none() {
            return Ok(());
        }
        let session = python::make_session_proxy(py, &self.config)?;
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
        let hook_funcs: Vec<Py<pyo3::PyAny>> = self
            .session
            .py_hooks
            .iter()
            .filter(|hook| hook.name == "pytest_sessionfinish")
            .map(|hook| hook.func.clone_ref(py))
            .collect();
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
