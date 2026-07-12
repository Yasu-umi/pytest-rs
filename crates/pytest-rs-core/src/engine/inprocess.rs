//! In-process nested sessions: backs pytester's `inline_run` by running a
//! fresh pytest session inside the already-running outer process (instead of
//! spawning the runner binary as a subprocess). Hooks fired during the nested
//! run go through the monitored pluggy relay so `HookRecorder.getcalls` can
//! observe live call objects — including custom hooks — which a subprocess
//! JSON relay can never carry.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};

use pyo3::prelude::*;

use crate::engine::Engine;
use crate::hooks::Plugin;

/// Depth of active in-process nested runs. While > 0, Rust hook dispatch
/// also notifies the plugin manager's call monitors (HookRecorder) with the
/// live kwargs, so getcalls works. Zero on the outer run — its hot-path
/// dispatch never crosses into Python to check for monitors.
static RECORDING_DEPTH: AtomicUsize = AtomicUsize::new(0);

/// True while inside an in-process nested run (recording hook calls).
pub fn recording() -> bool {
    RECORDING_DEPTH.load(Ordering::Relaxed) > 0
}

/// Brackets a nested run: bumps the recording depth for its lifetime so
/// re-entrancy (a nested run inside a nested run) is counted correctly.
pub(crate) struct RecordingGuard;

impl RecordingGuard {
    pub(crate) fn enter() -> Self {
        RECORDING_DEPTH.fetch_add(1, Ordering::Relaxed);
        RecordingGuard
    }
}

impl Drop for RecordingGuard {
    fn drop(&mut self) {
        RECORDING_DEPTH.fetch_sub(1, Ordering::Relaxed);
    }
}

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
        Err(message) if message.starts_with(crate::EXIT_ZERO_SENTINEL) => {
            let content = &message[crate::EXIT_ZERO_SENTINEL.len()..];
            if !content.is_empty() {
                print!("{}", content);
            }
            return Err(PyErr::new::<pyo3::exceptions::PySystemExit, _>(0_i32));
        }
        Err(message) => {
            let exc = py
                .import("pytest")?
                .getattr("UsageError")?
                .call1((message,))?;
            return Err(PyErr::from_value(exc));
        }
    };

    // `-p no:NAME` / PYTEST_RS_DISABLE_PLUGINS disable a bundled plugin
    // (mirror of main.rs, via the shared helper).
    plugins.retain(|plugin| !crate::plugin_is_disabled(plugin.name(), &config.plugin_opts));

    let mut engine = Engine::new(plugins, config);
    Ok(engine.run_nested(py))
}
