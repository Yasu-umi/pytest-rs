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
    #[cfg(feature = "benchmark")]
    plugins.push(Box::new(pytest_rs_benchmark::BenchmarkPlugin::new()));
    // Native plugins fire in this order. Affected-test selection runs
    // before pytest-split's hooks, so the split divides the selected set;
    // and after the other plugins' pytest_configure, so their setup (e.g.
    // benchmark's machine info, which starts a subprocess) is not recorded
    // as part of every test's dependencies.
    #[cfg(feature = "affected")]
    plugins.push(Box::new(pytest_rs_affected::AffectedPlugin::new()));
    #[cfg(feature = "split")]
    plugins.push(Box::new(pytest_rs_split::SplitPlugin::new()));
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

/// `--affected-merge A B ... --affected-data OUT`: no session, no tests.
#[cfg(feature = "affected")]
fn affected_merge(config: &Config) -> i32 {
    let Some(output) = config.get_value("affected-data") else {
        eprintln!("ERROR: --affected-merge needs --affected-data OUT\n");
        return pytest_rs_core::report::exit_code::USAGE_ERROR;
    };
    let inputs: Vec<std::path::PathBuf> = config
        .paths
        .iter()
        .map(|path| config.invocation_dir.join(path))
        .collect();
    match pytest_rs_affected::merge_files(&inputs, &config.invocation_dir.join(output)) {
        Ok(summary) => {
            println!("{summary}");
            pytest_rs_core::report::exit_code::OK
        }
        Err(message) => {
            eprintln!("ERROR: {message}\n");
            pytest_rs_core::report::exit_code::USAGE_ERROR
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
            eprintln!("ERROR: {message}\n");
            std::process::exit(pytest_rs_core::report::exit_code::USAGE_ERROR);
        }
    };

    #[cfg(feature = "affected")]
    if config.get_flag("affected-merge") {
        std::process::exit(affected_merge(&config));
    }

    #[cfg(feature = "xdist")]
    if config.get_flag("looponfail") {
        if config.get_flag("pdb") {
            eprintln!("ERROR: --pdb is incompatible with --looponfail.\n");
            std::process::exit(pytest_rs_core::report::exit_code::USAGE_ERROR);
        }
        std::process::exit(pytest_rs_core::looponfail::run(&config, &argv));
    }

    // `-p no:NAME` disables a bundled plugin at runtime (pytest semantics);
    // both the short name (no:cov) and the distribution-style name
    // (no:pytest_cov / no:pytest-cov) are accepted. The PYTEST_RS_DISABLE_PLUGINS
    // env var disables the same way but survives into nested pytester runs
    // (see pytest_rs_core::plugin_is_disabled).
    plugins
        .retain(|plugin| !pytest_rs_core::plugin_is_disabled(plugin.name(), &config.plugin_opts));

    let mut engine = Engine::new(plugins, config);
    let code = Python::attach(|py| engine.run(py));
    // Run Python shutdown hooks in the same order CPython does during
    // Py_Finalize: threading._shutdown() first (runs threading._register_atexit
    // callbacks — e.g. loky's _python_exit which terminates joblib worker
    // processes), then atexit._run_exitfuncs() (coverage writes its data here,
    // multiprocessing.util._exit_function joins child processes, etc.). Calling
    // them in the wrong order (or skipping threading._shutdown) causes
    // multiprocessing.util._exit_function to block for minutes on os.waitpid
    // waiting for workers that were never told to exit.
    //
    // After that, flush the Python streams, then terminate with libc::_exit.
    // We deliberately skip C-level atexit / C++ static destructors: some native
    // extension modules (e.g. duckdb) crash in those destructors under our
    // embedded interpreter, which never runs Py_Finalize. _exit avoids them and
    // the OS reclaims the process anyway. std::process::exit would run them and
    // segfault.
    Python::attach(|py| {
        if let Ok(m) = py.import("threading") {
            let _ = m.call_method0("_shutdown");
        }
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

#[cfg(test)]
mod tests {
    use super::build_plugins;

    /// `--affected` starts recording in its pytest_configure: a native
    /// plugin configured after it must not start subprocesses there (that
    /// would make every test's dependencies incomplete). Only pytest-split
    /// follows it, and it must, so the split divides the selected tests.
    #[test]
    #[cfg(all(feature = "affected", feature = "split"))]
    fn affected_is_registered_right_before_split_and_split_is_last() {
        let names: Vec<String> = build_plugins()
            .iter()
            .map(|plugin| plugin.name().to_string())
            .collect();
        let n = names.len();
        assert_eq!(&names[n - 2..], ["affected", "split"], "{names:?}");
    }
}
