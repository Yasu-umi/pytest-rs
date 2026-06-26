//! pytest-mock equivalent: the `mocker` fixture family (an embedded Python
//! shim wrapping unittest.mock) plus assert-method traceback wrapping.
//!
//! The shim is written to the per-run shim dir as a real `pytest_mock`
//! package so upstream `import pytest_mock` / `pytest_mock._util` resolve
//! through normal import machinery (and assertion rewriting applies to it,
//! like real pytest rewrites entry-point plugin modules).

use pytest_rs_core::hooks::{HookContext, Plugin};
use pytest_rs_core::pyo3::exceptions::{PyOSError, PyRuntimeError};
use pytest_rs_core::pyo3::prelude::*;
use pytest_rs_core::pyo3::types::PyModule;

const SHIM_FILES: &[(&str, &str)] = &[
    ("__init__.py", include_str!("../py/pytest_mock/__init__.py")),
    ("_util.py", include_str!("../py/pytest_mock/_util.py")),
    ("plugin.py", include_str!("../py/pytest_mock/plugin.py")),
];

pub struct MockPlugin {
    plugin_module: Option<Py<PyModule>>,
}

impl MockPlugin {
    pub fn new() -> Self {
        Self {
            plugin_module: None,
        }
    }

    fn plugin_module<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyModule>> {
        self.plugin_module
            .as_ref()
            .map(|m| m.bind(py).clone())
            .ok_or_else(|| PyRuntimeError::new_err("mock plugin not configured"))
    }
}

impl Default for MockPlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl Plugin for MockPlugin {
    fn name(&self) -> &str {
        "mock"
    }

    fn pytest_configure(&mut self, ctx: &mut HookContext) -> PyResult<()> {
        let py = ctx.py;
        let package_root = pytest_rs_core::python::shim_root().join("pytest_mock");
        std::fs::create_dir_all(&package_root).map_err(|e| PyOSError::new_err(e.to_string()))?;
        for (rel, content) in SHIM_FILES {
            let path = package_root.join(rel);
            if path.exists() {
                continue;
            }
            std::fs::write(path, content).map_err(|e| PyOSError::new_err(e.to_string()))?;
        }

        // Rewrite asserts inside the shim: introspection messages from
        // wrap_assert_methods rely on rewritten `assert a == b` diffs.
        py.import("pytest")?
            .getattr("register_assert_rewrite")?
            .call1(("pytest_mock",))?;
        let plugin_module = py.import("pytest_mock.plugin")?;

        // mock_traceback_monkeypatch (default true) wraps the assert_called_*
        // family to hide tracebacks and add introspection, unless --tb=native.
        let config = pytest_rs_core::python::make_py_config(py, ctx.config)?;
        let tb = ctx.config.get_value("tb").unwrap_or("auto");
        plugin_module.getattr("_configure")?.call1((config, tb))?;

        let package = py.import("pytest_mock")?;
        pytest_rs_core::python::register_plugin_fixtures(py, &package, &mut ctx.session.registry)?;
        self.plugin_module = Some(plugin_module.unbind());
        Ok(())
    }

    fn pytest_sessionfinish(&mut self, ctx: &mut HookContext, _exit_code: i32) -> PyResult<()> {
        self.plugin_module(ctx.py)?
            .getattr("_unconfigure")?
            .call0()?;
        Ok(())
    }
}
