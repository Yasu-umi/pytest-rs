//! In-process nested sessions: backs pytester's `inline_run` by running a
//! fresh pytest session inside the already-running outer process (instead of
//! spawning the runner binary as a subprocess). Hooks fired during the nested
//! run go through the monitored pluggy relay so `HookRecorder.getcalls` can
//! observe live call objects — including custom hooks — which a subprocess
//! JSON relay can never carry.

use std::sync::OnceLock;

use pyo3::prelude::*;

use crate::engine::Engine;
use crate::hooks::Plugin;

/// Builds the bundled plugin set. Registered by the binary crate at startup,
/// since the concrete plugin set (feature-gated) lives there, not in core.
pub type PluginFactory = fn() -> Vec<Box<dyn Plugin>>;

static PLUGIN_FACTORY: OnceLock<PluginFactory> = OnceLock::new();

/// Register the plugin factory used to build a fresh plugin set for each
/// in-process nested run. Called once from `main` before the outer run.
pub fn register_plugin_factory(factory: PluginFactory) {
    let _ = PLUGIN_FACTORY.set(factory);
}

/// Run a fresh pytest session in-process and return its exit code.
///
/// The caller (`pytest._pytester`) owns the parts that must bracket this
/// call: snapshotting `sys.modules`/`sys.path`/cwd, swapping in a fresh
/// global capture state, redirecting fds 1/2 to collect the terminal output,
/// and registering a `HookRecorder` on the plugin manager beforehand.
pub fn run_inprocess(py: Python<'_>, args: Vec<String>) -> PyResult<i32> {
    let factory = PLUGIN_FACTORY.get().ok_or_else(|| {
        pyo3::exceptions::PyRuntimeError::new_err(
            "pytest-rs: no plugin factory registered for in-process runs",
        )
    })?;
    let mut plugins = factory();

    let mut parser = crate::config::OptionParser::default();
    for plugin in &plugins {
        plugin.pytest_addoption(&mut parser);
    }

    let mut argv = vec!["pytest-rs".to_string()];
    argv.extend(args);
    let config = match crate::config::Config::from_args(parser, argv) {
        Ok(config) => config,
        Err(message) => {
            let exc = py
                .import("pytest")?
                .getattr("UsageError")?
                .call1((message,))?;
            return Err(PyErr::from_value(exc));
        }
    };

    // `-p no:NAME` disables a bundled plugin (mirror of main.rs).
    plugins.retain(|plugin| {
        !config.plugin_opts.iter().any(|spec| {
            spec.strip_prefix("no:").is_some_and(|disabled| {
                disabled
                    .trim_start_matches("pytest_")
                    .trim_start_matches("pytest-")
                    == plugin.name()
            })
        })
    });

    let mut engine = Engine::new(plugins, config);
    Ok(engine.run_nested(py))
}
