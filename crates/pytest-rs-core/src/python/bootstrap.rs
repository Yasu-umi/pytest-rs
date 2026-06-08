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
        // Built-in plugin names ("pytester", ...) aren't importable modules;
        // they're provided natively, so only importable plugins register.
        let plugin = match py.import(name.as_str()) {
            Ok(plugin) => plugin,
            // Under `python -m pytest` the invocation dir is sys.path[0],
            // so -p resolves local plugin modules; emulate that for the
            // import only, then drop the path entry again.
            Err(_) => {
                let Some(dir) = search_dir else { continue };
                let dir = dir.to_string_lossy();
                let sys_path = py.import("sys")?.getattr("path")?;
                sys_path.call_method1("insert", (0, dir.as_ref()))?;
                let result = py.import(name.as_str());
                let _ = sys_path.call_method1("remove", (dir.as_ref(),));
                let Ok(plugin) = result else { continue };
                plugin
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
        py.import("pytest._pluginmanager")?
            .getattr("pluginmanager")?
            .call_method1("register", (&plugin,))?;
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
    // pluginmanager's hook relay, LIFO like pluggy.
    py.import("pytest._pluginmanager")?
        .getattr("pluginmanager")?
        .call_method1("register", (&module,))?;
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
