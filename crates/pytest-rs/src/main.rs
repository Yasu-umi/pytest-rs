use pytest_rs_core::pyo3::Python;
use pytest_rs_core::pyo3::types::PyAnyMethods;
use pytest_rs_core::{Config, Engine, OptionParser, Plugin};

// Feature-gated pushes; vec![] cannot hold cfg'd elements.
#[allow(clippy::vec_init_then_push)]
fn build_plugins() -> Vec<Box<dyn Plugin>> {
    let mut plugins: Vec<Box<dyn Plugin>> = Vec::new();
    #[cfg(feature = "asyncio")]
    plugins.push(Box::new(pytest_rs_asyncio::AsyncioPlugin::new()));
    #[cfg(feature = "anyio")]
    plugins.push(Box::new(pytest_rs_anyio::AnyioPlugin::new()));
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

// libc::_exit below is the one unsafe call; see the comment at the exit site.
#[allow(unsafe_code)]
fn main() {
    preinitialize_python();
    // Explicit interpreter startup (no pyo3 auto-initialize): required so the
    // binary can embed libpython statically for distribution.
    Python::initialize();
    // In-process nested runs (pytester inline_run) build a fresh plugin set
    // through this factory; the concrete (feature-gated) set lives here.
    pytest_rs_core::register_plugin_factory(build_plugins);
    let mut plugins = build_plugins();

    let mut parser = OptionParser::default();
    for plugin in &plugins {
        plugin.pytest_addoption(&mut parser);
    }

    let argv: Vec<String> = std::env::args().collect();
    let config = match Config::from_args(parser, argv.clone()) {
        Ok(config) => config,
        Err(message) if message.starts_with(pytest_rs_core::EXIT_ZERO_SENTINEL) => {
            print!("{}", &message[pytest_rs_core::EXIT_ZERO_SENTINEL.len()..]);
            std::process::exit(0);
        }
        Err(message) => {
            eprintln!("ERROR: {message}");
            std::process::exit(pytest_rs_core::report::exit_code::USAGE_ERROR);
        }
    };

    #[cfg(feature = "xdist")]
    if config.get_flag("looponfail") {
        if config.get_flag("pdb") {
            eprintln!("ERROR: --pdb is incompatible with --looponfail.");
            std::process::exit(pytest_rs_core::report::exit_code::USAGE_ERROR);
        }
        std::process::exit(pytest_rs_core::looponfail::run(&config, &argv));
    }

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
    // Run Python atexit handlers (coverage writes its data here, etc.) and flush
    // the Python streams, then terminate with libc::_exit. We deliberately skip
    // C-level atexit / C++ static destructors: some native extension modules
    // (e.g. duckdb) crash in those destructors under our embedded interpreter,
    // which never runs Py_Finalize. _exit avoids them and the OS reclaims the
    // process anyway. std::process::exit would run them and segfault.
    Python::attach(|py| {
        if let Ok(m) = py.import("atexit") {
            let _ = m.call_method0("_run_exitfuncs");
        }
        if let Ok(sys) = py.import("sys") {
            for stream in ["stdout", "stderr"] {
                if let Ok(s) = sys.getattr(stream) {
                    let _ = s.call_method0("flush");
                }
            }
        }
    });
    use std::io::Write as _;
    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();
    // SAFETY: _exit just terminates the process; it is async-signal-safe and
    // takes no action that can be unsound here.
    unsafe { libc::_exit(code) }
}
