//! Interpreter bootstrap: shim install, plugin/conftest loading, py hook calls.

#[allow(unused_imports)]
use super::*;
use crate::collect::{file_nodeid, module_name_for};
use crate::fixture::FixtureRegistry;
use pyo3::types::{PyDict, PyModule};
use std::path::{Path, PathBuf};

/// An embedded interpreter does not activate a virtualenv on its own: when
/// VIRTUAL_ENV is set, add its site-packages via site.addsitedir — which
/// also processes .pth files (editable installs), unlike PYTHONPATH — and
/// move the added entries to the front so the venv shadows the base env.
pub fn activate_virtualenv(py: Python<'_>) -> PyResult<()> {
    py.run(
        c"
import glob as _glob
import os as _os
import site as _site
import sys as _sys

_venv = _os.environ.get('VIRTUAL_ENV')
if _venv:
    _candidates = _glob.glob(_os.path.join(_venv, 'lib', 'python*', 'site-packages'))
    _candidates.append(_os.path.join(_venv, 'Lib', 'site-packages'))
    for _dir in _candidates:
        if _os.path.isdir(_dir) and _dir not in _sys.path:
            _before = len(_sys.path)
            _site.addsitedir(_dir)
            _added = _sys.path[_before:]
            del _sys.path[_before:]
            _sys.path[:0] = _added
    # Real pytest run from a venv has sys.executable = the venv python;
    # tests spawning subprocesses through sys.executable expect the venv
    # site-packages to be importable there.
    for _exe in (('bin', 'python'), ('Scripts', 'python.exe')):
        _candidate = _os.path.join(_venv, *_exe)
        if _os.path.isfile(_candidate):
            _sys.executable = _candidate
            break
",
        None,
        None,
    )
}

/// Set up the embedded interpreter for a run: write the pytest shim package
/// to a temp dir and prepend it to sys.path so `import pytest` resolves to us.
pub fn install_shim(py: Python<'_>) -> PyResult<PathBuf> {
    let shim_root = shim_root();
    for (rel, content) in SHIM_FILES {
        let path = shim_root.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| pyo3::exceptions::PyOSError::new_err(e.to_string()))?;
        }
        std::fs::write(&path, content)
            .map_err(|e| pyo3::exceptions::PyOSError::new_err(e.to_string()))?;
    }
    sys_path_prepend(py, &shim_root)?;

    // Under real pytest, `__main__` is the console script (or pytest's
    // `__main__.py` under `python -m pytest`) and always has a `__file__`.
    // The embedded interpreter's `__main__` has none, which breaks code
    // that monkeypatches it (e.g. anyio's to_process tests). Point it at
    // the shim's `pytest/__main__.py` — a real file with the upstream
    // name-guard, so re-importing it as `__mp_main__` (multiprocessing,
    // anyio workers) is a no-op `import pytest`.
    py.run(
        c"import sys
_main = sys.modules['__main__']
if not hasattr(_main, '__file__'):
    import os
    _main.__file__ = os.path.join(_shim_root, 'pytest', '__main__.py')
",
        None,
        Some(&{
            let locals = pyo3::types::PyDict::new(py);
            locals.set_item("_shim_root", shim_root.to_string_lossy())?;
            locals
        }),
    )?;

    // Expose the Rust-backed request type for `_pytest.fixtures` imports.
    let pytest_module = py.import("pytest")?;
    pytest_module.setattr("FixtureRequest", py.get_type::<crate::request::PyRequest>())?;
    // Pre-import modules that teardown (finalize_generator) imports lazily.
    // If a test replaces sys.implementation with a mock object that lacks
    // `cache_tag`, any fresh module import during teardown will fail with
    // AttributeError because the import machinery calls
    // importlib._bootstrap_external.cache_from_source() → sys.implementation.cache_tag.
    // _pytest.fixtures pulls in `inspect` which transitively imports `tokenize`
    // (and linecache); forcing them here ensures they are in sys.modules before
    // any test can monkey-patch sys.implementation.
    py.import("_pytest.fixtures")?;
    // Register a minimal plugin that provides pytest_runtest_makereport
    // default through the hook relay (plugins like pytest-subtests call
    // item.ihook.pytest_runtest_makereport).
    py.run(
        c"import _pytest.runner as _r
import pytest._pluginmanager as _pm
class _MakeReportPlugin:
    @staticmethod
    def pytest_runtest_makereport(item, call):
        return _r.pytest_runtest_makereport(item, call)
_pm.pluginmanager.register(_MakeReportPlugin(), '_pytest.runner')
",
        None,
        None,
    )?;
    // The pytest-rs crate version, for the session-header "pytest-rs-X" tag
    // (a replacement TerminalReporter must match the native header).
    pytest_module.setattr("_rs_version", env!("CARGO_PKG_VERSION"))?;
    // pytest.Config: the config type, for annotations/isinstance (pytest-django).
    pytest_module.setattr("Config", py.get_type::<crate::request::PyConfig>())?;

    // Native config builder backing `pytester.parseconfig(*args)`: builds a
    // fresh in-process Config from command-line args (rootdir discovery, ini
    // reading, option parsing). Injected as a closure since the engine
    // exposes no Python extension module.
    let prepareconfig = pyo3::types::PyCFunction::new_closure(
        py,
        Some(c"_native_prepareconfig"),
        None,
        |args: &Bound<'_, pyo3::types::PyTuple>,
         _kwargs: Option<&Bound<'_, pyo3::types::PyDict>>|
         -> PyResult<Py<PyAny>> {
            let py = args.py();
            let arglist: Vec<String> = args.get_item(0)?.extract()?;
            super::prepare_config(py, arglist)
        },
    )?;
    py.import("_pytest.config")?
        .setattr("_native_prepareconfig", prepareconfig)?;

    // In-process nested run backing `pytester.inline_run`: builds a fresh
    // config + plugin set from args and runs a whole session in this process.
    // Returns the exit code; the Python wrapper handles sys.* snapshots, fd
    // capture and HookRecorder registration around it.
    let inline_run = pyo3::types::PyCFunction::new_closure(
        py,
        Some(c"_native_inline_run"),
        None,
        |args: &Bound<'_, pyo3::types::PyTuple>,
         _kwargs: Option<&Bound<'_, pyo3::types::PyDict>>|
         -> PyResult<Py<PyAny>> {
            let py = args.py();
            let arglist: Vec<String> = args.get_item(0)?.extract()?;
            let code = crate::engine::inprocess::run_inprocess(py, arglist)?;
            Ok(code.into_pyobject(py)?.into_any().unbind())
        },
    )?;
    pytest_module.setattr("_native_inline_run", inline_run)?;

    // runtestprotocol delegation (pytest-rerunfailures): the re-entrant phase
    // runner and the logreport capture sink that records what a delegated
    // protocol logs. Both are no-ops outside a delegated run.
    let run_phases = pyo3::types::PyCFunction::new_closure(
        py,
        Some(c"_native_run_item_phases"),
        None,
        |args: &Bound<'_, pyo3::types::PyTuple>,
         _kwargs: Option<&Bound<'_, pyo3::types::PyDict>>|
         -> PyResult<Py<PyAny>> { crate::runner::run_item_phases(args.py()) },
    )?;
    let capture = pyo3::types::PyCFunction::new_closure(
        py,
        Some(c"_native_capture_logreport"),
        None,
        |args: &Bound<'_, pyo3::types::PyTuple>,
         _kwargs: Option<&Bound<'_, pyo3::types::PyDict>>|
         -> PyResult<Py<PyAny>> {
            let py = args.py();
            let report = args.get_item(0)?;
            let captured = crate::runner::capture_logreport(py, &report)?;
            Ok(pyo3::types::PyBool::new(py, captured)
                .to_owned()
                .into_any()
                .unbind())
        },
    )?;
    let runner_mod = py.import("_pytest.runner")?;
    runner_mod.setattr("_native_run_item_phases", run_phases)?;
    runner_mod.setattr("_native_capture_logreport", capture)?;
    // Register the module-level sink so ihook.pytest_runtest_logreport reaches it.
    let sink = runner_mod.getattr("_logreport_sink")?;
    py.import("pytest._pluginmanager")?
        .getattr("pluginmanager")?
        .call_method1("register", (sink, "_logreport_sink"))?;

    // Assertion rewriting: rewrite `assert` in test modules at import time.
    py.import("pytest._rewrite")?.call_method0("install")?;

    // The embedded interpreter never runs Python's exit machinery, so a
    // block-buffered (piped) stdout would silently drop test prints.
    py.run(
        c"import sys
for _stream in (sys.stdout, sys.stderr):
    try:
        _stream.reconfigure(line_buffering=True)
    except (AttributeError, ValueError):
        pass
",
        None,
        None,
    )?;

    // The pytester fixture spawns this binary to run nested sessions.
    if let Ok(exe) = std::env::current_exe() {
        py.import("os")?
            .getattr("environ")?
            .set_item("PYTEST_RS_EXE", exe.to_string_lossy())?;
    }

    // Embedded interpreters inherit argv[0] as sys.executable, which would
    // make `subprocess.run([sys.executable, ...])` re-run pytest-rs
    // recursively. Point it at the real python binary instead.
    py.run(
        c"import os, sys, sysconfig
if not os.path.basename(sys.executable).startswith('python'):
    for _name in ('python' + sysconfig.get_config_var('VERSION'), 'python3'):
        _candidate = os.path.join(sysconfig.get_config_var('BINDIR'), _name)
        if os.path.exists(_candidate):
            sys.executable = _candidate
            break
",
        None,
        None,
    )?;
    Ok(shim_root)
}

/// `pytest_plugins = "name"` / `pytest_plugins = (...)` in a test module:
/// import each named module and register its hooks and fixtures globally.
/// Bundled plugins (pytest_asyncio, ...) resolve to their shims and carry
/// no pytest_* hooks, so importing them here is a no-op.
pub(crate) fn register_pytest_plugins(
    py: Python<'_>,
    module: &Bound<'_, PyModule>,
    registry: &mut FixtureRegistry,
    hooks: &mut Vec<crate::session::PyHook>,
) -> PyResult<()> {
    let Ok(declared) = module.getattr("pytest_plugins") else {
        return Ok(());
    };
    let names: Vec<String> = match declared.extract::<String>() {
        Ok(single) => vec![single],
        Err(_) => declared
            .try_iter()?
            .map(|name| name?.extract::<String>())
            .collect::<PyResult<_>>()?,
    };
    if let Ok(rewrite_mod) = py.import("pytest._rewrite") {
        let py_names = pyo3::types::PyTuple::new(py, &names)?;
        let _ = rewrite_mod.call_method1("register_assert_rewrite", py_names);
    }
    load_named_plugins(py, &names, None, registry, hooks)
}

/// Import plugin modules by name (`-p NAME` / `pytest_plugins`) and register
/// their fixtures and pytest_* hooks globally.
pub fn load_named_plugins(
    py: Python<'_>,
    names: &[String],
    search_dir: Option<&Path>,
    registry: &mut FixtureRegistry,
    hooks: &mut Vec<crate::session::PyHook>,
) -> PyResult<()> {
    for name in names {
        // Try the name directly, then as `_pytest.{name}` (for built-in
        // pytest plugin short-names like "pytester"), then via the search_dir.
        let plugin = match py.import(name.as_str()) {
            Ok(plugin) => plugin,
            Err(_) => {
                let scoped = format!("_pytest.{name}");
                match py.import(scoped.as_str()) {
                    Ok(plugin) => plugin,
                    Err(_) => {
                        // Under `python -m pytest` the invocation dir is
                        // sys.path[0], so -p resolves local plugin modules;
                        // emulate that for the import only.
                        let Some(dir) = search_dir else { continue };
                        let dir = dir.to_string_lossy();
                        let sys_path = py.import("sys")?.getattr("path")?;
                        sys_path.call_method1("insert", (0, dir.as_ref()))?;
                        let result = py.import(name.as_str());
                        let _ = sys_path.call_method1("remove", (dir.as_ref(),));
                        let Ok(plugin) = result else { continue };
                        plugin
                    }
                }
            }
        };
        // Re-registering an already-seen plugin would duplicate its hooks.
        let already = hooks
            .iter()
            .any(|hook| hook.plugin_module.as_deref() == Some(name.as_str()));
        if already {
            continue;
        }
        register_fixtures_from(py, &plugin, "", registry)?;
        let before = hooks.len();
        scan_py_hooks(&plugin, "", hooks)?;
        for hook in &mut hooks[before..] {
            hook.plugin_module = Some(name.clone());
        }
    }
    Ok(())
}

/// Distributions whose pytest11 plugin must not autoload under the shim:
/// the bundled set (pytest-rs replaces them natively) plus plugins known
/// to require real pytest internals at hook time. hypothesis works fully
/// as a library without its plugin (which only adds reporting/CLI
/// integration but calls pytest.Function & co. from its hooks).
pub(crate) const SKIPPED_DISTS: [&str; 7] = [
    "pytest-asyncio",
    "pytest-mock",
    "pytest-cov",
    "pytest-split",
    "pytest-benchmark",
    "pytest-xdist",
    "hypothesis",
];

/// Auto-load installed third-party plugins (`pytest11` entry points),
/// pytest's setuptools-plugin loading. `blocked` carries `-p no:NAME`
/// names (matched against the entry-point name and its module);
/// PYTEST_DISABLE_PLUGIN_AUTOLOAD disables the pass entirely. Unlike
/// pytest, a plugin that fails to import warns (PytestConfigWarning) and
/// is skipped instead of aborting the run: plugins built against real
/// pytest internals would otherwise make every run unusable.
pub fn load_entrypoint_plugins(
    py: Python<'_>,
    blocked: &[String],
    registry: &mut FixtureRegistry,
    hooks: &mut Vec<crate::session::PyHook>,
    distinfo: &mut Vec<String>,
) -> PyResult<()> {
    if std::env::var_os("PYTEST_DISABLE_PLUGIN_AUTOLOAD").is_some() {
        return Ok(());
    }
    let globals = pyo3::types::PyDict::new(py);
    globals.set_item("bundled", SKIPPED_DISTS.to_vec())?;
    // (entry-point name, plugin module, dist Name, dist version): the dist
    // metadata feeds the "plugins:" header line (pytest's _plugin_nameversions).
    py.run(
        c"from importlib.metadata import distributions\n\
bundled = {name.lower() for name in bundled}\n\
result = sorted({(ep.name, ep.value.split(':')[0].strip(), (dist.metadata.get('Name') if dist.metadata else None) or '', dist.version or '') for dist in distributions() if ((dist.metadata.get('Name') if dist.metadata else None) or '').lower() not in bundled for ep in dist.entry_points if ep.group == 'pytest11'})\n",
        Some(&globals),
        None,
    )?;
    let entrypoints: Vec<(String, String, String, String)> = globals
        .get_item("result")?
        .map(|result| result.extract())
        .transpose()?
        .unwrap_or_default();

    for (ep_name, module_name, dist_name, dist_version) in entrypoints {
        if blocked.contains(&ep_name) || blocked.contains(&module_name) {
            continue;
        }
        // Already loaded via -p NAME or a conftest's pytest_plugins.
        let already = hooks
            .iter()
            .any(|hook| hook.plugin_module.as_deref() == Some(module_name.as_str()));
        if already {
            continue;
        }
        let plugin = match py.import(module_name.as_str()) {
            Ok(plugin) => plugin,
            Err(err) => {
                let _ = warn_explicit_at(
                    py,
                    "PytestConfigWarning",
                    &format!("could not load plugin '{ep_name}': {}", err.value(py)),
                    module_name.as_str(),
                    0,
                );
                continue;
            }
        };
        register_fixtures_from(py, &plugin, "", registry)?;
        let before = hooks.len();
        scan_py_hooks(&plugin, "", hooks)?;
        for hook in &mut hooks[before..] {
            hook.plugin_module = Some(module_name.clone());
        }
        // Track the module in the shim pluginmanager so its custom-hook
        // impls are reachable via config.pluginmanager.hook.<name> and its
        // pytest_addhooks specs register (pluggy registration parity).
        // Register under the entry-point name (as pytest does) so
        // config.pluginmanager.getplugin("<ep_name>") resolves the module
        // (pytest-mypy's conftest patches plugin.MypyFileItem this way).
        py.import("pytest._pluginmanager")?
            .getattr("pluginmanager")?
            .call_method1("register", (&plugin, ep_name.as_str()))?;
        // The "plugins:" header label: pytest strips a leading "pytest-"
        // from the dist name and appends the version (_plugin_nameversions),
        // deduped.
        if !dist_name.is_empty() {
            let label = dist_name.strip_prefix("pytest-").unwrap_or(&dist_name);
            let entry = format!("{label}-{dist_version}");
            if !distinfo.contains(&entry) {
                distinfo.push(entry);
            }
        }
    }
    Ok(())
}

/// Import a conftest.py; its fixtures and pytest_* hooks are visible to
/// all items under its directory.
pub fn collect_conftest(
    py: Python<'_>,
    rootdir: &Path,
    path: &Path,
    registry: &mut FixtureRegistry,
    hooks: &mut Vec<crate::session::PyHook>,
) -> PyResult<()> {
    let (basedir, module_name) = module_name_for(path);
    sys_path_prepend(py, &basedir)?;
    // Conftests in nested directories (without __init__.py) all resolve to
    // the module name "conftest"; a plain import would alias them to the
    // first one imported. Import shadowed ones under a unique name instead.
    let module = match conftest_alias_name(py, &module_name, path)? {
        Some(unique) => import_module_from_path(py, &unique, path)?,
        None => py.import(module_name.as_str())?,
    };
    let dir_nodeid = file_nodeid(rootdir, path.parent().unwrap_or(rootdir));
    let baseid = if dir_nodeid.is_empty() || dir_nodeid == "." {
        String::new()
    } else {
        format!("{dir_nodeid}/")
    };
    // `pytest_plugins = [...]` in a conftest: the named modules' fixtures
    // and hooks register globally. Dotted names ("tests.fixtures.db")
    // resolve against the rootdir.
    sys_path_prepend(py, rootdir)?;
    register_pytest_plugins(py, &module, registry, hooks)?;
    register_fixtures_from(py, &module, &baseid, registry)?;
    scan_py_hooks(&module, &baseid, hooks)?;
    // Conftests are plugins too: custom-hook impls they define (e.g. an
    // override of pytest_timeout_set_timer) dispatch through the shim
    // pluginmanager's hook relay, LIFO like pluggy. Pass the conftest
    // path as the registration name so _importconftest's getplugin check
    // finds the already-loaded module and avoids a fresh import (which
    // would produce a distinct module object missing any mutated state).
    py.import("pytest._pluginmanager")?
        .getattr("pluginmanager")?
        .call_method1("register", (&module, path.to_string_lossy().as_ref()))?;
    // Populate _dirpath2confmods so pytester's _getconftestmodules(path)
    // returns the conftest module even after nested-run cleanup restores
    // _conftest_plugins.
    let kwargs = pyo3::types::PyDict::new(py);
    kwargs.set_item("consider_namespace_packages", true)?;
    let pathlib = py.import("pathlib")?.getattr("Path")?;
    let py_path = pathlib.call1((path.to_string_lossy().as_ref(),))?;
    let py_rootdir = pathlib.call1((rootdir.to_string_lossy().as_ref(),))?;
    let _ = py
        .import("pytest._pluginmanager")?
        .getattr("pluginmanager")?
        .call_method(
            "_loadconftestmodules",
            (&py_path, "prepend", &py_rootdir),
            Some(&kwargs),
        );
    Ok(())
}

/// A unique import name for a conftest whose module name is already taken
/// by a *different* file in sys.modules; None when a plain import is safe.
pub(crate) fn conftest_alias_name(
    py: Python<'_>,
    module_name: &str,
    path: &Path,
) -> PyResult<Option<String>> {
    let sys_modules = py.import("sys")?.getattr("modules")?;
    let Ok(existing) = sys_modules.get_item(module_name) else {
        return Ok(None);
    };
    let same_file = existing
        .getattr("__file__")
        .ok()
        .and_then(|file| file.extract::<String>().ok())
        .map(|file| {
            let existing_path = std::fs::canonicalize(&file).unwrap_or_else(|_| file.into());
            let this_path = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
            existing_path == this_path
        })
        .unwrap_or(false);
    if same_file {
        return Ok(None);
    }
    let suffix: String = path
        .parent()
        .map(|dir| dir.to_string_lossy().replace(['/', '\\', '.', ':'], "_"))
        .unwrap_or_default();
    Ok(Some(format!("{module_name}@{suffix}")))
}

/// Import a module from an explicit file path under the given name
/// (importlib spec machinery; registers it in sys.modules).
pub(crate) fn import_module_from_path<'py>(
    py: Python<'py>,
    name: &str,
    path: &Path,
) -> PyResult<Bound<'py, PyModule>> {
    let sys_modules = py.import("sys")?.getattr("modules")?;
    if let Ok(existing) = sys_modules.get_item(name) {
        return existing.cast_into::<PyModule>().map_err(Into::into);
    }
    let util = py.import("importlib.util")?;
    let spec = util
        .getattr("spec_from_file_location")?
        .call1((name, path.to_string_lossy().as_ref()))?;
    let module = util.getattr("module_from_spec")?.call1((&spec,))?;
    sys_modules.set_item(name, &module)?;
    spec.getattr("loader")?
        .call_method1("exec_module", (&module,))?;
    module.cast_into::<PyModule>().map_err(Into::into)
}

/// Scan a conftest module for pytest_* hook functions.
pub fn scan_py_hooks(
    module: &Bound<'_, PyModule>,
    baseid: &str,
    hooks: &mut Vec<crate::session::PyHook>,
) -> PyResult<()> {
    for (key, value) in module.dict().iter() {
        let Ok(name) = key.extract::<String>() else {
            continue;
        };
        if name.starts_with("pytest_") && value.is_callable() {
            hooks.push(crate::session::PyHook {
                name,
                func: value.unbind(),
                baseid: baseid.to_string(),
                plugin_module: None,
            });
        }
    }
    Ok(())
}

/// During an in-process nested run, notify the plugin manager's call
/// monitors (a HookRecorder) of a hook invocation with its live kwargs, so
/// `getcalls` observes the call — including custom hooks the native engine
/// dispatches directly rather than through pluggy. No-op on the outer run
/// (recording depth is zero, so this never crosses into Python).
pub fn record_hook(py: Python<'_>, name: &str, available: &[(&str, Py<PyAny>)]) {
    if !crate::engine::inprocess::recording() {
        return;
    }
    let kwargs = PyDict::new(py);
    for (key, value) in available {
        let _ = kwargs.set_item(key, value.bind(py));
    }
    let _ = py
        .import("pytest._pluginmanager")
        .and_then(|m| m.getattr("pluginmanager"))
        .and_then(|pm| pm.call_method1("record_hook", (name, kwargs)));
}

/// Call a conftest/plugin hook with only the keyword arguments its
/// signature requests, without driving generator results — callers that
/// wrap a phase (hookwrappers) advance/finish the generator themselves.
pub fn call_py_hook_raw(
    py: Python<'_>,
    func: &Py<PyAny>,
    available: &[(&str, Py<PyAny>)],
) -> PyResult<Py<PyAny>> {
    let func = func.bind(py);
    let accepted = param_names(py, func)?;
    let kwargs = PyDict::new(py);
    for (name, value) in available {
        if accepted.iter().any(|param| param == name) {
            kwargs.set_item(name, value.bind(py))?;
        }
    }
    let empty = pyo3::types::PyTuple::empty(py);
    Ok(func.call(empty, Some(&kwargs))?.unbind())
}

/// Call a conftest hook with only the keyword arguments its signature
/// requests. Generator hooks (pytest wrapper style: `return (yield)`) are
/// driven to completion: setup before the yield, the rest right after.
pub fn call_py_hook(
    py: Python<'_>,
    func: &Py<PyAny>,
    available: &[(&str, Py<PyAny>)],
) -> PyResult<Py<PyAny>> {
    let result = call_py_hook_raw(py, func, available)?;
    let result = result.bind(py);

    let inspect = py.import("inspect")?;
    let is_generator: bool = inspect
        .getattr("isgenerator")?
        .call1((result,))?
        .extract()?;
    if !is_generator {
        return Ok(result.clone().unbind());
    }
    // Drive the wrapper: run to the yield, then to completion.
    let next_fn = py.import("builtins")?.getattr("next")?;
    if let Err(err) = next_fn.call1((result,)) {
        if err.is_instance_of::<pyo3::exceptions::PyStopIteration>(py) {
            return Ok(py.None());
        }
        return Err(err);
    }
    match result.call_method1("send", (py.None(),)) {
        Ok(_) => Err(pyo3::exceptions::PyRuntimeError::new_err(
            "conftest hook wrapper yielded more than once",
        )),
        Err(err) if err.is_instance_of::<pyo3::exceptions::PyStopIteration>(py) => Ok(py.None()),
        Err(err) => Err(err),
    }
}

/// Extract collect_ignore and collect_ignore_glob entries from a conftest module.
/// conftest_path is the absolute path to the conftest.py file; we look it up
/// in sys.modules by matching __file__. conftest_dir is the directory of that
/// conftest; relative paths in collect_ignore are resolved relative to it.
/// Returns (paths, globs).
pub fn extract_collect_ignores(
    py: Python<'_>,
    conftest_dir: &Path,
    conftest_path: &Path,
) -> (Vec<PathBuf>, Vec<String>) {
    let conftest_str = conftest_path.to_string_lossy();
    let conftest_canonical =
        std::fs::canonicalize(conftest_path).unwrap_or_else(|_| conftest_path.to_path_buf());

    let Ok(sys_modules) = py.import("sys").and_then(|s| s.getattr("modules")) else {
        return (Vec::new(), Vec::new());
    };
    // Find the conftest module by matching __file__
    let module = {
        let mut found = None;
        if let Ok(values) = sys_modules.call_method0("values") {
            for m in values.try_iter().into_iter().flatten().flatten() {
                if let Ok(file_attr) = m.getattr("__file__")
                    && let Ok(file_str) = file_attr.extract::<String>()
                {
                    let canon = std::fs::canonicalize(&file_str)
                        .unwrap_or_else(|_| std::path::PathBuf::from(&file_str));
                    if canon == conftest_canonical || file_str == conftest_str.as_ref() {
                        found = Some(m);
                        break;
                    }
                }
            }
        }
        match found {
            Some(m) => m,
            None => return (Vec::new(), Vec::new()),
        }
    };

    let mut paths = Vec::new();
    let mut globs = Vec::new();

    // collect_ignore: list of path-like objects, resolved relative to conftest_dir
    if let Ok(ignore_list) = module.getattr("collect_ignore")
        && let Ok(iter) = ignore_list.try_iter()
    {
        for item in iter.flatten() {
            // os.fspath converts PathLike → str/bytes; fallback to str()
            let path_str: Option<String> = py
                .import("os")
                .and_then(|os| os.getattr("fspath"))
                .and_then(|fsp| fsp.call1((&item,)))
                .and_then(|s| s.extract::<String>())
                .or_else(|_| item.str().and_then(|s| s.extract::<String>()))
                .ok();
            if let Some(s) = path_str {
                let abs = conftest_dir.join(&s);
                paths.push(abs);
            }
        }
    }

    // collect_ignore_glob: list of glob pattern strings
    if let Ok(glob_list) = module.getattr("collect_ignore_glob")
        && let Ok(iter) = glob_list.try_iter()
    {
        for item in iter.flatten() {
            if let Ok(s) = item.str().and_then(|s| s.extract::<String>()) {
                globs.push(s);
            }
        }
    }

    (paths, globs)
}

/// Call pytest_ignore_collect for a path; returns true if the path should be
/// ignored. Only calls hooks whose baseid is a prefix of the path (conftests
/// only ignore paths within their subtree).
/// Check whether a path should be ignored by pytest_ignore_collect hooks.
///
/// Returns:
/// - `None` = do not ignore
/// - `Some(None)` = ignore silently (hook returned truthy)
/// - `Some(Some(reason))` = ignore and emit a skip report (hook raised Skipped)
pub fn call_ignore_collect_hooks(
    py: Python<'_>,
    hooks: &[crate::session::PyHook],
    path: &Path,
    rootdir: &Path,
) -> Option<Option<String>> {
    let ignore_hooks: Vec<&crate::session::PyHook> = hooks
        .iter()
        .filter(|h| h.name == "pytest_ignore_collect")
        .collect();
    if ignore_hooks.is_empty() {
        return None;
    }
    let pathlib = match py.import("pathlib").and_then(|m| m.getattr("Path")) {
        Ok(p) => p,
        Err(_) => return None,
    };
    let py_path = match pathlib.call1((path.to_string_lossy().as_ref(),)) {
        Ok(p) => p,
        Err(_) => return None,
    };
    // Build a minimal config proxy for the hook parameter
    let config = crate::python::proxies::existing_py_config(py).map(|c| c.into_bound(py));
    for hook in &ignore_hooks {
        // Only apply hooks from conftests whose baseid is relevant to this path
        // (baseid is "" for root conftest, "subdir" for subdir conftest).
        let hook_dir = if hook.baseid.is_empty() {
            rootdir.to_path_buf()
        } else {
            rootdir.join(&hook.baseid)
        };
        if !path.starts_with(&hook_dir) {
            continue;
        }
        let result = call_py_hook_raw(
            py,
            &hook.func,
            &[
                ("collection_path", py_path.clone().unbind()),
                (
                    "config",
                    config
                        .as_ref()
                        .map(|c| c.clone().unbind())
                        .unwrap_or_else(|| py.None()),
                ),
                ("path", py_path.clone().unbind()),
            ],
        );
        match result {
            Ok(result) => {
                if result.bind(py).is_truthy().unwrap_or(false) {
                    return Some(None);
                }
            }
            // pytest.skip() in pytest_ignore_collect: ignore the path and emit a skip report
            Err(ref err)
                if err
                    .get_type(py)
                    .name()
                    .map(|n| n == "Skipped")
                    .unwrap_or(false) =>
            {
                let reason = err
                    .value(py)
                    .getattr("msg")
                    .and_then(|m| m.extract::<String>())
                    .unwrap_or_else(|_| "Skipped".to_string());
                return Some(Some(reason));
            }
            Err(_) => {}
        }
    }
    None
}
