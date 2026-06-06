use pytest_rs_core::pyo3::Python;
use pytest_rs_core::{Config, Engine, OptionParser, Plugin};

// Feature-gated pushes; vec![] cannot hold cfg'd elements.
#[allow(clippy::vec_init_then_push)]
fn build_plugins() -> Vec<Box<dyn Plugin>> {
    let mut plugins: Vec<Box<dyn Plugin>> = Vec::new();
    #[cfg(feature = "asyncio")]
    plugins.push(Box::new(pytest_rs_asyncio::AsyncioPlugin::new()));
    #[cfg(feature = "mock")]
    plugins.push(Box::new(pytest_rs_mock::MockPlugin::new()));
    #[cfg(feature = "cov")]
    plugins.push(Box::new(pytest_rs_cov::CovPlugin::new()));
    #[cfg(feature = "split")]
    plugins.push(Box::new(pytest_rs_split::SplitPlugin::new()));
    #[cfg(feature = "benchmark")]
    plugins.push(Box::new(pytest_rs_benchmark::BenchmarkPlugin::new()));
    plugins
}

/// Pre-initialize Python with the CLI's locale handling. pyo3's
/// auto-initialize uses Py_InitializeEx (the legacy compat config), which
/// skips PEP 538 C-locale coercion and PEP 540 UTF-8 mode — open() then
/// defaults to ascii in LANG-less containers. PyPreConfig_InitPythonConfig
/// restores the `python` binary's behavior (PYTHONUTF8 honored, UTF-8 mode
/// auto-enabled under the C/POSIX locale).
#[allow(unsafe_code)]
fn preinitialize_python() {
    use pytest_rs_core::pyo3::ffi;
    // SAFETY: runs once at startup, before any Python use or thread spawns.
    unsafe {
        let mut preconfig: ffi::PyPreConfig = std::mem::zeroed();
        ffi::PyPreConfig_InitPythonConfig(&mut preconfig);
        let status = ffi::Py_PreInitialize(&preconfig);
        if ffi::PyStatus_Exception(status) != 0 {
            // Pre-init failing is non-fatal: Py_InitializeEx falls back to
            // its own (compat) pre-initialization.
            eprintln!("warning: Python pre-initialization failed; locale coercion disabled");
        }
    }
}

fn main() {
    preinitialize_python();
    // Explicit interpreter startup (no pyo3 auto-initialize): required so the
    // binary can embed libpython statically for distribution.
    Python::initialize();
    let mut plugins = build_plugins();

    let mut parser = OptionParser::default();
    for plugin in &plugins {
        plugin.pytest_addoption(&mut parser);
    }

    let argv: Vec<String> = std::env::args().collect();
    let config = match Config::from_args(parser, argv) {
        Ok(config) => config,
        Err(message) => {
            eprintln!("{message}");
            std::process::exit(pytest_rs_core::report::exit_code::USAGE_ERROR);
        }
    };

    // `-p no:NAME` disables a bundled plugin at runtime (pytest semantics);
    // both the short name (no:cov) and the distribution-style name
    // (no:pytest_cov / no:pytest-cov) are accepted.
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
    let code = Python::attach(|py| engine.run(py));
    std::process::exit(code);
}
