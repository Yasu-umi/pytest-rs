//! The Python-visible `request` fixture and `config` proxy objects.

use std::collections::HashMap;
use std::sync::Mutex;

use pyo3::prelude::*;

/// A (subset of the) pytest `Config` API passed to conftest hooks.
/// `dict`: plugins set ad-hoc attributes on config (pytest-timeout's
/// `config._env_timeout`), like upstream's plain-Python Config.
#[pyclass(name = "Config", dict)]
pub struct PyConfig {
    rootdir: String,
    /// Full path to the discovered config file (pytest.ini / pyproject.toml
    /// / tox.ini / setup.cfg), or None when no config file was found.
    inipath: Option<String>,
    /// The resolved collection arguments (`config.args`): the path-like CLI
    /// tokens, or the testpaths/invocation-dir fallback.
    args: Vec<String>,
    /// `config.args_source`: "args" (explicit CLI paths), "testpaths" (from
    /// the testpaths ini), or "invocation_dir" (the default).
    args_source: String,
    ini: Mutex<HashMap<String, String>>,
    /// Raw ini file values (without -o overrides), for `config._inicfg`.
    ini_file: HashMap<String, String>,
    /// Raw -o override values, kept separately so Python's `getini` can
    /// perform alias-aware override resolution (alias in `-o` should win).
    /// Mutex-protected so `config.parse()` can add new overrides post-init.
    ini_overrides: Mutex<HashMap<String, String>>,
    /// Strict getini: an unregistered, non-core ini key raises ValueError
    /// (upstream behavior). Only parseconfig-built configs are strict; the
    /// session config stays lenient since the engine owns the core inis.
    strict: bool,
    /// The argparse-namespace equivalent (`config.option`), mutable from
    /// Python so conftest hooks can stash flags on it.
    #[pyo3(get)]
    option: Py<PyAny>,
    /// Lazily-created `pytest.Stash`; the config proxy is a session
    /// singleton, so plugin data stored here (e.g. pytest-timeout's
    /// session deadline) persists across hooks.
    stash: pyo3::sync::PyOnceLock<Py<PyAny>>,
    /// Lazily-created per-config PluginManager for parseconfig contexts
    /// (`strict = true`). Parseconfig tests register plugins here; the
    /// session config uses the global shared PM instead.
    local_pm: pyo3::sync::PyOnceLock<Py<PyAny>>,
    /// Optional override for `workerinput`, set by conftest hooks in tests
    /// that simulate xdist worker processes (`config.workerinput = True`).
    workerinput_override: std::sync::Mutex<Option<Py<PyAny>>>,
    /// The original CLI arguments before parsing (for `config.invocation_params`).
    original_args: Vec<String>,
    /// Cleanup callbacks registered via `config.add_cleanup`.
    cleanups: Mutex<Vec<Py<PyAny>>>,
    /// Lazily-created TagTracer for `config.trace`.
    trace_obj: pyo3::sync::PyOnceLock<Py<PyAny>>,
    /// Set to true by `_mark_as_parsed()` (called after parseconfig fully
    /// initialises the config) so a second `parse()` raises AssertionError.
    parsed: std::sync::atomic::AtomicBool,
}

impl PyConfig {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        rootdir: String,
        inipath: Option<String>,
        args: Vec<String>,
        args_source: String,
        ini: HashMap<String, String>,
        ini_file: HashMap<String, String>,
        ini_overrides: HashMap<String, String>,
        option: Py<PyAny>,
        strict: bool,
    ) -> Self {
        Self {
            rootdir,
            inipath,
            args,
            args_source,
            ini: Mutex::new(ini),
            ini_file,
            ini_overrides: Mutex::new(ini_overrides),
            strict,
            option,
            stash: pyo3::sync::PyOnceLock::new(),
            local_pm: pyo3::sync::PyOnceLock::new(),
            workerinput_override: std::sync::Mutex::new(None),
            original_args: Vec::new(),
            cleanups: Mutex::new(Vec::new()),
            trace_obj: pyo3::sync::PyOnceLock::new(),
            parsed: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Resolve a CLI name to its option dest: "-X" / "--foo" → "foo" via the
    /// registered flag_dests aliases; plain "foo" or unknown names fall back to
    /// stripping leading dashes and replacing `-` with `_`.
    fn opt2dest(py: Python<'_>, name: &str) -> PyResult<String> {
        let flag_dests = py.import("pytest._parser")?.getattr("flag_dests")?;
        if let Ok(dest) = flag_dests
            .get_item(name)
            .and_then(|d| d.extract::<String>())
        {
            return Ok(dest);
        }
        Ok(name.trim_start_matches('-').replace('-', "_"))
    }

    /// Validate that every `-o`/`--override-ini` in `args` is followed by a
    /// non-flag value within those same args (upstream: `_validate_args`).
    /// `source` is included in the UsageError message for user context.
    fn check_parse_override_ini(py: Python<'_>, args: &[String], source: &str) -> PyResult<()> {
        let mut i = 0;
        while i < args.len() {
            let arg = &args[i];
            if arg == "-o" || arg == "--override-ini" {
                let next = args.get(i + 1);
                if next.is_none() || next.is_some_and(|s| s.starts_with('-')) {
                    let msg = format!(
                        "error: argument -o/--override-ini: expected one argument\n  config source: {source}"
                    );
                    let exc = py.import("pytest")?.getattr("UsageError")?.call1((msg,))?;
                    return Err(PyErr::from_value(exc));
                }
                i += 2;
                continue;
            }
            i += 1;
        }
        Ok(())
    }
}

#[pymethods]
impl PyConfig {
    /// `config._inicfg`: raw ini file values (without -o overrides) wrapped in
    /// ConfigValue objects. Keys from overrides appear with `origin="override"`.
    #[getter]
    fn _inicfg(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        let config_value_cls = py
            .import("_pytest.config.findpaths")?
            .getattr("ConfigValue")?;
        let is_toml = self
            .inipath
            .as_deref()
            .map(|p| p.ends_with(".toml"))
            .unwrap_or(false);
        let mode = if is_toml { "toml" } else { "ini" };
        let dict = pyo3::types::PyDict::new(py);
        for (k, v) in &self.ini_file {
            let kw = pyo3::types::PyDict::new(py);
            kw.set_item("origin", "file")?;
            kw.set_item("mode", mode)?;
            dict.set_item(k, config_value_cls.call((v,), Some(&kw))?)?;
        }
        let ini_overrides = self
            .ini_overrides
            .lock()
            .expect("ini_overrides lock poisoned");
        for (k, v) in ini_overrides.iter() {
            let kw = pyo3::types::PyDict::new(py);
            kw.set_item("origin", "override")?;
            kw.set_item("mode", "ini")?;
            dict.set_item(k, config_value_cls.call((v,), Some(&kw))?)?;
        }
        Ok(dict.unbind().into_any())
    }

    #[getter]
    fn stash(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        Ok(self
            .stash
            .get_or_try_init(py, || -> PyResult<Py<PyAny>> {
                Ok(py
                    .import("pytest._stash")?
                    .getattr("Stash")?
                    .call0()?
                    .unbind())
            })?
            .clone_ref(py))
    }

    /// pytest's Config.issue_config_time_warning: a warning raised during
    /// configure (no test to attribute it to); the session warning capture
    /// records it for the warnings summary.
    #[pyo3(signature = (warning, stacklevel = 2))]
    fn issue_config_time_warning(
        &self,
        py: Python<'_>,
        warning: Py<PyAny>,
        stacklevel: i32,
    ) -> PyResult<()> {
        let kwargs = pyo3::types::PyDict::new(py);
        kwargs.set_item("stacklevel", stacklevel)?;
        py.import("warnings")?
            .call_method("warn", (warning,), Some(&kwargs))?;
        Ok(())
    }

    fn getini(&self, py: Python<'_>, name: &str) -> PyResult<Py<PyAny>> {
        // Pass the full ini snapshot so the resolver can apply alias lookups
        // and type coercion (parser.addini specs supply both); paths/pathlist
        // types resolve relative to rootdir. Overrides are passed separately
        // so the Python layer can give them precedence over file values even
        // when the override key is an alias of the canonical name.
        let inicfg = pyo3::types::PyDict::new(py);
        {
            let ini = self.ini.lock().expect("config lock poisoned");
            for (key, value) in ini.iter() {
                inicfg.set_item(key, value)?;
            }
        }
        let overrides = pyo3::types::PyDict::new(py);
        {
            let ini_overrides = self
                .ini_overrides
                .lock()
                .expect("ini_overrides lock poisoned");
            for (key, value) in ini_overrides.iter() {
                overrides.set_item(key, value)?;
            }
        }
        let kwargs = pyo3::types::PyDict::new(py);
        kwargs.set_item("overrides", &overrides)?;
        Ok(py
            .import("pytest._parser")?
            .call_method(
                "getini",
                (name, inicfg, self.rootdir.as_str(), self.strict),
                Some(&kwargs),
            )?
            .unbind())
    }

    /// `config._get_unknown_ini_keys()`: ini-file keys that aren't a
    /// registered or core option (pytest's unknown-config-option set).
    fn _get_unknown_ini_keys(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        let keys: Vec<String> = self
            .ini
            .lock()
            .expect("config lock poisoned")
            .keys()
            .cloned()
            .collect();
        Ok(py
            .import("pytest._parser")?
            .call_method1("unknown_ini_keys", (keys,))?
            .unbind())
    }

    /// Append one line to a line-list ini option (e.g. "markers").
    fn addinivalue_line(&self, name: &str, line: &str) {
        let mut ini = self.ini.lock().expect("config lock poisoned");
        let entry = ini.entry(name.to_string()).or_default();
        if !entry.is_empty() {
            entry.push('\n');
        }
        entry.push_str(line);
    }

    #[pyo3(signature = (name, default = None, skip = false))]
    fn getoption(
        &self,
        py: Python<'_>,
        name: &str,
        default: Option<Py<PyAny>>,
        skip: bool,
    ) -> PyResult<Py<PyAny>> {
        let dest = Self::opt2dest(py, name)?;
        let ns = self.option.bind(py);
        match ns.getattr(dest.as_str()) {
            Ok(val) if val.is_none() && skip => {
                // Registered option with None value + skip=True: use default or pytest.skip().
                if let Some(d) = default {
                    return Ok(d);
                }
                py.import("pytest")?
                    .call_method1("skip", (format!("no {name:?} option found"),))?;
                unreachable!()
            }
            Ok(val) => Ok(val.unbind()),
            Err(_) => {
                // AttributeError: option not declared in namespace.
                if let Some(d) = default {
                    return Ok(d);
                }
                if skip {
                    py.import("pytest")?
                        .call_method1("skip", (format!("no {name:?} option found"),))?;
                    unreachable!()
                }
                Err(PyErr::new::<pyo3::exceptions::PyValueError, _>(format!(
                    "no option named {name:?}"
                )))
            }
        }
    }

    #[pyo3(signature = (name, default = None))]
    fn getvalue(
        &self,
        py: Python<'_>,
        name: &str,
        default: Option<Py<PyAny>>,
    ) -> PyResult<Py<PyAny>> {
        self.getoption(py, name, default, false)
    }

    /// `config.getvalueorskip(name)`: the option's value, or skip the test if
    /// it is unset/None (pytest's getoption(..., skip=True)).
    fn getvalueorskip(&self, py: Python<'_>, name: &str) -> PyResult<Py<PyAny>> {
        self.getoption(py, name, None, true)
    }

    /// `config.get_verbosity(type)`: the level for a fine-grained verbosity
    /// type, or the global verbose level when the type is unknown/unset.
    #[pyo3(signature = (verbosity_type = None))]
    fn get_verbosity(&self, py: Python<'_>, verbosity_type: Option<String>) -> PyResult<Py<PyAny>> {
        let global_level = self.option.bind(py).getattr("verbose")?.unbind();
        let Some(vt) = verbosity_type else {
            return Ok(global_level);
        };
        let ini_name = format!("verbosity_{vt}");
        // The verbosity ini must be registered (a plugin's addini); otherwise
        // fall back to the global level (upstream's `_parser._inidict` check).
        let registered = py
            .import("pytest._parser")?
            .getattr("ini_specs")?
            .contains(ini_name.as_str())?;
        if !registered {
            return Ok(global_level);
        }
        let level = self.getini(py, &ini_name)?;
        if level.bind(py).extract::<String>().ok().as_deref() == Some("auto") {
            return Ok(global_level);
        }
        Ok(py
            .import("builtins")?
            .getattr("int")?
            .call1((level.bind(py),))?
            .unbind())
    }

    /// xdist parity: present (with the worker id) only in -n workers, so
    /// `hasattr(config, "workerinput")` detects worker processes. Custom
    /// entries set by controller-side pytest_configure_node hooks arrive
    /// as JSON via PYTEST_RS_WORKERINPUT. Also settable by conftest hooks
    /// (e.g. test_stepwise sets `config.workerinput = True` to simulate a
    /// worker without actually running xdist).
    #[getter]
    fn workerinput<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        // Check for an explicit override set via the setter.
        if let Ok(guard) = self.workerinput_override.lock()
            && let Some(ref v) = *guard
        {
            return Ok(v.bind(py).clone());
        }
        match std::env::var("PYTEST_XDIST_WORKER") {
            Ok(worker_id) => {
                let dict = pyo3::types::PyDict::new(py);
                dict.set_item("workerid", worker_id)?;
                if let Ok(count) = std::env::var("PYTEST_XDIST_WORKER_COUNT") {
                    dict.set_item("workercount", count.parse::<usize>().unwrap_or(1))?;
                }
                if let Ok(uid) = std::env::var("PYTEST_XDIST_TESTRUNUID") {
                    dict.set_item("testrunuid", uid)?;
                }
                if let Ok(json) = std::env::var("PYTEST_RS_WORKERINPUT")
                    && let Ok(extra) = py.import("json")?.call_method1("loads", (json,))
                {
                    dict.call_method1("update", (extra,))?;
                }
                Ok(dict.into_any())
            }
            Err(_) => Err(pyo3::exceptions::PyAttributeError::new_err("workerinput")),
        }
    }

    #[setter]
    fn set_workerinput(&self, py: Python<'_>, value: Py<PyAny>) -> PyResult<()> {
        if let Ok(mut guard) = self.workerinput_override.lock() {
            *guard = Some(value.clone_ref(py));
        }
        Ok(())
    }

    /// xdist parity: a worker-only dict the controller receives back in
    /// pytest_testnodedown (node.workeroutput).
    #[getter]
    fn workeroutput<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        if std::env::var("PYTEST_XDIST_WORKER").is_err() {
            return Err(pyo3::exceptions::PyAttributeError::new_err("workeroutput"));
        }
        py.import("pytest._dist")?.getattr("workeroutput")
    }

    #[getter]
    fn rootpath<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        py.import("pathlib")?
            .getattr("Path")?
            .call1((self.rootdir.as_str(),))
    }

    #[getter]
    fn rootdir<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        self.rootpath(py)
    }

    /// `config.inipath`: a Path to the discovered config file, or None.
    #[getter]
    fn inipath<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        match &self.inipath {
            Some(path) => py
                .import("pathlib")?
                .getattr("Path")?
                .call1((path.as_str(),)),
            None => Ok(py.None().into_bound(py)),
        }
    }

    /// `config.inifile`: legacy py.path alias for inipath (deprecated form).
    #[getter]
    fn inifile<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        match &self.inipath {
            Some(path) => py
                .import("pytest._tmp_path")?
                .getattr("LocalPath")?
                .call1((path.as_str(),)),
            None => Ok(py.None().into_bound(py)),
        }
    }

    #[getter]
    fn args(&self, py: Python<'_>) -> Py<PyAny> {
        pyo3::types::PyList::new(py, &self.args)
            .map(|list| list.into_any().unbind())
            .unwrap_or_else(|_| py.None())
    }

    /// `config.args_source`: the upstream `Config.ArgsSource` enum member
    /// (ARGS / TESTPATHS / INVOCATION_DIR) matching how `args` was derived.
    #[getter]
    fn args_source<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let member = match self.args_source.as_str() {
            "args" => "ARGS",
            "testpaths" => "TESTPATHS",
            _ => "INVOCATION_DIR",
        };
        py.import("_pytest.config")?
            .getattr("Config")?
            .getattr("ArgsSource")?
            .getattr(member)
    }

    /// pytest's parseconfigure step: fire pytest_configure. Kept minimal —
    /// the cache/getini/getoption surface parseconfig tests exercise needs
    /// no plugin configuration, and re-firing the shared pluginmanager's
    /// hooks would reconfigure the outer session's plugins.
    fn _do_configure(&self) {}

    /// pytest's config teardown: run registered cleanup callbacks (LIFO).
    fn _ensure_unconfigure(&self, py: Python<'_>) -> PyResult<()> {
        let callbacks: Vec<Py<PyAny>> = {
            let mut guard = self.cleanups.lock().expect("cleanups lock poisoned");
            std::mem::take(&mut *guard)
        };
        let mut errors: Vec<PyErr> = Vec::new();
        for cb in callbacks.into_iter().rev() {
            if let Err(e) = cb.bind(py).call0() {
                errors.push(e);
            }
        }
        if let Some(e) = errors.pop() {
            return Err(e);
        }
        Ok(())
    }

    /// Store original CLI args (called by prepare_config after parsing).
    fn _set_invocation_args(&mut self, args: Vec<String>) {
        self.original_args = args;
    }

    /// `config.invocation_params`: namedtuple with the original CLI args,
    /// plugins list, and invocation directory.
    #[getter]
    fn invocation_params<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let collections = py.import("collections")?;
        let cls =
            collections.call_method1("namedtuple", ("InvocationParams", "args plugins dir"))?;
        let args_tuple = pyo3::types::PyTuple::new(py, &self.original_args)?;
        let plugins_tuple = pyo3::types::PyTuple::empty(py);
        let invocation_dir = py
            .import("pathlib")?
            .getattr("Path")?
            .call1((".",))?
            .call_method0("resolve")?;
        cls.call1((args_tuple, plugins_tuple, invocation_dir))
    }

    /// `config.trace`: a TagTracer-compatible callable whose `.root` has
    /// `setwriter(fn)`, and calling `config.trace(msg)` writes
    /// `"msg [config]\n"` through the writer.
    #[getter]
    fn trace<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        Ok(self
            .trace_obj
            .get_or_try_init(py, || -> PyResult<Py<PyAny>> {
                Ok(py
                    .import("pytest._config_trace")?
                    .getattr("ConfigTrace")?
                    .call0()?
                    .unbind())
            })?
            .bind(py)
            .clone())
    }

    /// Register a cleanup function called during `_ensure_unconfigure`.
    /// Can be used as a decorator: `@config.add_cleanup`.
    fn add_cleanup(&self, py: Python<'_>, func: Py<PyAny>) -> PyResult<Py<PyAny>> {
        self.cleanups
            .lock()
            .expect("cleanups lock poisoned")
            .push(func.clone_ref(py));
        Ok(func)
    }

    /// `config._getconftest_pathlist(name, path)`: read a conftest variable
    /// and resolve its entries as paths relative to the conftest's directory.
    #[pyo3(signature = (name, path = None))]
    fn _getconftest_pathlist(
        &self,
        py: Python<'_>,
        name: &str,
        path: Option<Py<PyAny>>,
    ) -> PyResult<Py<PyAny>> {
        Ok(py
            .import("pytest._config_trace")?
            .call_method1("getconftest_pathlist", (name, path, self.rootdir.as_str()))?
            .unbind())
    }

    /// Mark this config as "already parsed" so a subsequent `parse()` call
    /// raises AssertionError (upstream parity for parseconfig-built configs).
    fn _mark_as_parsed(&self) {
        self.parsed
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }

    /// `config.parse(args, addopts=True)`: apply `args` to this config,
    /// optionally processing `PYTEST_ADDOPTS` first (upstream parity).
    /// Extracts `-o key=value` pairs and updates `ini_overrides`/`ini`.
    #[pyo3(signature = (args, addopts = true))]
    fn parse(&self, py: Python<'_>, args: Vec<String>, addopts: bool) -> PyResult<()> {
        if self.parsed.load(std::sync::atomic::Ordering::Relaxed) {
            return Err(pyo3::exceptions::PyAssertionError::new_err(
                "can only parse cmdline args at most once per Config object",
            ));
        }
        self.parsed
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let shlex = py.import("shlex")?;
        let mut combined: Vec<String> = Vec::new();

        if addopts {
            if let Ok(env_opts) = std::env::var("PYTEST_ADDOPTS") {
                let env_opts = env_opts.trim().to_string();
                if !env_opts.is_empty() {
                    let env_args: Vec<String> =
                        shlex.call_method1("split", (&env_opts,))?.extract()?;
                    // Validate env args in isolation (upstream: _validate_args)
                    Self::check_parse_override_ini(py, &env_args, "via PYTEST_ADDOPTS")?;
                    combined.extend(env_args);
                }
            }
        }
        combined.extend(args);

        // Extract -o key=value pairs from the combined args
        let mut new_overrides: Vec<(String, String)> = Vec::new();
        let mut i = 0;
        while i < combined.len() {
            let arg = &combined[i];
            if arg == "-o" || arg == "--override-ini" {
                if let Some(kv) = combined.get(i + 1) {
                    if let Some((k, v)) = kv.split_once('=') {
                        new_overrides.push((k.to_string(), v.to_string()));
                        i += 2;
                        continue;
                    }
                }
            } else if let Some(rest) = arg.strip_prefix("--override-ini=") {
                if let Some((k, v)) = rest.split_once('=') {
                    new_overrides.push((k.to_string(), v.to_string()));
                }
            }
            i += 1;
        }

        // Apply overrides to ini_overrides and the merged ini map
        {
            let mut ini_overrides = self
                .ini_overrides
                .lock()
                .expect("ini_overrides lock poisoned");
            let mut ini = self.ini.lock().expect("config lock poisoned");
            for (k, v) in new_overrides {
                ini.insert(k.clone(), v.clone());
                ini_overrides.insert(k, v);
            }
        }
        Ok(())
    }

    /// `config.notify_exception(excinfo, option)`: fire the
    /// `pytest_internalerror` hook; if no handler returns True, write the
    /// exception repr to stderr (pytest's default behaviour).
    #[pyo3(signature = (excinfo, option = None))]
    fn notify_exception(
        &self,
        py: Python<'_>,
        excinfo: Py<PyAny>,
        option: Option<Py<PyAny>>,
    ) -> PyResult<()> {
        let _ = option;
        let repr = excinfo.bind(py).call_method0("getrepr")?;
        let hook_relay = self.hook(py)?;
        let hook_caller = hook_relay.getattr("pytest_internalerror")?;
        let kwargs = pyo3::types::PyDict::new(py);
        kwargs.set_item("excrepr", &repr)?;
        let handled = hook_caller.call((), Some(&kwargs))?;
        let any_true = handled
            .try_iter()
            .map(|mut iter| iter.any(|v| v.is_ok_and(|v| v.is_truthy().unwrap_or(false))))
            .unwrap_or(false);
        if !any_true {
            let stderr = py.import("sys")?.getattr("stderr")?;
            let repr_str = repr.str()?.to_str()?.to_string();
            for line in repr_str.lines() {
                stderr.call_method1("write", (format!("INTERNALERROR> {line}\n"),))?;
            }
            let _ = stderr.call_method0("flush");
        }
        Ok(())
    }

    /// `config.inicfg`: a mutable dict view of the ini-file values.
    /// Mutations to the returned dict do not propagate back to the config
    /// (matches pytest's recent behaviour where inicfg is a detached view).
    #[getter]
    fn inicfg(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        let dict = pyo3::types::PyDict::new(py);
        for (key, value) in self.ini.lock().expect("config lock poisoned").iter() {
            dict.set_item(key, value)?;
        }
        Ok(dict.into_any().unbind())
    }

    /// Minimal pluginmanager (getplugin("logging-plugin") etc.).
    /// Parseconfig configs (`strict = true`) get their own fresh PM so that
    /// test-registered plugins (e.g. `config.pluginmanager.register(A())`)
    /// don't bleed into the session and the global terminal plugin doesn't
    /// intercept `pytest_internalerror` before the test can check stderr.
    #[getter]
    fn pluginmanager<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        if self.strict {
            let pm = self
                .local_pm
                .get_or_try_init(py, || -> PyResult<Py<PyAny>> {
                    Ok(py
                        .import("pytest._pluginmanager")?
                        .getattr("PluginManager")?
                        .call0()?
                        .unbind())
                })?;
            Ok(pm.bind(py).clone())
        } else {
            py.import("pytest._pluginmanager")?.getattr("pluginmanager")
        }
    }

    /// config.hook: the pluggy-lite hook relay (config.hook.<name>(**kw)
    /// dispatches to every registered plugin, e.g. sugar's header calling
    /// config.hook.pytest_report_header).
    #[getter]
    fn hook<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        self.pluginmanager(py)?.getattr("hook")
    }

    /// A TerminalWriter on the ORIGINAL stdout (sys.__stdout__, fd 1) —
    /// upstream's is created before capture replaces sys.stdout, so
    /// out-of-band dumps (pytest-timeout's stack dump before os._exit)
    /// reach the real terminal once capture is suspended.
    fn get_terminal_writer<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let stdout = py.import("sys")?.getattr("__stdout__")?;
        let file = if stdout.is_none() {
            py.None().into_bound(py)
        } else {
            stdout
        };
        py.import("_pytest._io")?
            .getattr("TerminalWriter")?
            .call1((file,))
    }

    /// The pytest `config.cache` API (a pytest._cache.Cache).
    #[getter]
    fn cache<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let cls = py.import("pytest._cache")?.getattr("Cache")?;
        let dir = cls.call_method1(
            "cache_dir_from",
            (
                self.rootdir.as_str(),
                self.ini
                    .lock()
                    .expect("config lock poisoned")
                    .get("cache_dir")
                    .cloned()
                    .unwrap_or_default(),
            ),
        )?;
        cls.call1((dir,))
    }
}

/// A (subset of the) pytest `FixtureRequest` API. Constructed per fixture
/// setup; finalizers registered through it are drained into the session's
/// finalizer stack by the resolver afterwards.
#[pyclass(name = "FixtureRequest")]
pub struct PyRequest {
    param: Option<Py<PyAny>>,
    node: Py<PyAny>,
    fixturename: Option<String>,
    scope: crate::fixture::Scope,
    finalizers: Mutex<Vec<Py<PyAny>>>,
    /// Lazily-built pytest-bdd FixtureManager view; once present, mutations
    /// (injected step fixtures / target_fixtures) persist across the request.
    fixturemanager: Mutex<Option<Py<PyAny>>>,
}

impl PyRequest {
    pub fn new(
        param: Option<Py<PyAny>>,
        node: Py<PyAny>,
        fixturename: Option<String>,
        scope: crate::fixture::Scope,
    ) -> Self {
        Self {
            param,
            node,
            fixturename,
            scope,
            finalizers: Mutex::new(Vec::new()),
            fixturemanager: Mutex::new(None),
        }
    }

    /// The cached pytest-bdd FixtureManager, if one was built for this request.
    fn manager(&self, py: Python<'_>) -> Option<Py<PyAny>> {
        self.fixturemanager
            .lock()
            .expect("request lock poisoned")
            .as_ref()
            .map(|fm| fm.clone_ref(py))
    }

    /// Resolve `argname` through the FixtureManager's `_arg2fixturedefs` if it
    /// holds a def for that name: a pinned `cached_result` (injected
    /// target_fixture) wins; otherwise an alias to a collected fixture
    /// (`registry_name`) resolves through the normal Rust path. Returns None
    /// when the manager has nothing for `argname` (fall back to the registry).
    fn resolve_via_manager(
        py: Python<'_>,
        fm: &Bound<'_, PyAny>,
        argname: &str,
    ) -> PyResult<Option<Py<PyAny>>> {
        let defs = fm
            .getattr("_arg2fixturedefs")?
            .call_method1("get", (argname,))?;
        if defs.is_none() {
            return Ok(None);
        }
        let len = defs.len().unwrap_or(0);
        if len == 0 {
            return Ok(None);
        }
        let def = defs.get_item(len - 1)?;
        if def.hasattr("cached_result")? {
            let cached = def.getattr("cached_result")?;
            return Ok(Some(cached.get_item(0)?.unbind()));
        }
        if let Ok(reg) = def.getattr("registry_name")
            && let Ok(Some(reg_name)) = reg.extract::<Option<String>>()
        {
            return Ok(Some(crate::runner::getfixturevalue(py, &reg_name)?));
        }
        Ok(None)
    }

    /// Finalizers registered via addfinalizer, in registration order.
    pub fn take_finalizers(&self) -> Vec<Py<PyAny>> {
        std::mem::take(&mut self.finalizers.lock().expect("request lock poisoned"))
    }
}

#[pymethods]
impl PyRequest {
    /// Engine use: run (and clear) the addfinalizer callbacks, LIFO. Called
    /// at the owning fixture's teardown so finalizers added late (factory
    /// fixtures calling addfinalizer during the test) are included.
    fn _drain_finalizers(&self, py: Python<'_>) -> PyResult<()> {
        // Run every finalizer (LIFO) even if some raise, collecting failures.
        // pytest re-raises a lone failure as-is, but groups multiple into a
        // BaseExceptionGroup so none are lost (#2440).
        let mut errors: Vec<PyErr> = Vec::new();
        for finalizer in self.take_finalizers().into_iter().rev() {
            if let Err(err) = finalizer.bind(py).call0() {
                errors.push(err);
            }
        }
        if errors.len() <= 1 {
            return match errors.pop() {
                Some(err) => Err(err),
                None => Ok(()),
            };
        }
        let argname = self.fixturename.clone().unwrap_or_default();
        let node = self
            .node
            .bind(py)
            .str()
            .map(|s| s.to_string())
            .unwrap_or_default();
        let msg = format!("errors while tearing down fixture \"{argname}\" of {node}");
        // pytest passes exceptions[::-1]: the finalizers run LIFO, so reversing
        // restores registration order in the reported group.
        let exc_values: Vec<Py<PyAny>> = errors
            .iter()
            .rev()
            .map(|err| err.value(py).clone().into_any().unbind())
            .collect();
        let exc_list = pyo3::types::PyList::new(py, exc_values)?;
        let group = py
            .import("builtins")?
            .getattr("BaseExceptionGroup")?
            .call1((msg, exc_list))?;
        Err(PyErr::from_value(group))
    }

    /// The current parameter for parametrized fixtures. AttributeError when
    /// the fixture is not parametrized, matching pytest.
    #[getter]
    fn param(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        match &self.param {
            Some(value) => Ok(value.clone_ref(py)),
            None => Err(pyo3::exceptions::PyAttributeError::new_err("param")),
        }
    }

    #[getter]
    fn fixturename(&self) -> Option<String> {
        self.fixturename.clone()
    }

    /// The `request.node` object (a pytest._node.Node shim instance).
    #[getter]
    fn node(&self, py: Python<'_>) -> Py<PyAny> {
        self.node.clone_ref(py)
    }

    /// The scope string of the fixture being requested ("function", "module",
    /// ...), matching pytest's `request.scope`.
    #[getter]
    fn scope(&self) -> &'static str {
        self.scope.as_str()
    }

    /// pytest's `request._pyfuncitem`: the underlying test Function item.
    /// Conftests reach for it to drive the fixture manager. For the common
    /// function-scoped request this is `request.node`; higher-scope requests
    /// expose their collector as `node` but the shim keeps the same handle.
    #[getter]
    fn _pyfuncitem(&self, py: Python<'_>) -> Py<PyAny> {
        self.node.clone_ref(py)
    }

    /// Names of all fixtures visible to this request's item, in pytest's
    /// scope-sorted closure order. Falls back to the node proxy's static list
    /// when no item is running (e.g. a request built outside a test run).
    #[getter]
    fn fixturenames(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        if let Some(names) = crate::runner::current_fixturenames(py) {
            return Ok(pyo3::types::PyList::new(py, names)?.into_any().unbind());
        }
        Ok(self.node.bind(py).getattr("fixturenames")?.unbind())
    }

    /// The item's keywords mapping (pytest's request.keywords == node.keywords).
    ///
    /// Scope-aware: session-scoped fixtures see only session-level keywords
    /// (mutations persist in _session_state); function-scoped fixtures see
    /// the item keywords merged with session-level keywords.
    #[getter]
    fn keywords(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        let session_kw = py
            .import("pytest._node")?
            .call_method0("get_session_keywords")?;
        if self.scope == crate::fixture::Scope::Session {
            return Ok(session_kw.unbind());
        }
        // For function/class/module/package scope: item keywords + session keywords.
        let item_kw = self.node.bind(py).getattr("keywords")?;
        // Start with item keywords and overlay session keywords (session wins on conflict).
        let merged = item_kw.call_method1("__or__", (session_kw,))?;
        Ok(merged.unbind())
    }

    /// The underlying test function object.
    /// Returns None for doctest items (they have no user-visible test function).
    #[getter]
    fn function(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        // Only a function-scoped request maps to a single test function;
        // wider scopes raise AttributeError like pytest.
        if self.scope > crate::fixture::Scope::Function {
            return Err(pyo3::exceptions::PyAttributeError::new_err(format!(
                "function not available in {}-scoped context",
                self.scope.as_str()
            )));
        }
        let node = self.node.bind(py);
        if node
            .getattr("_pytest_doctest_item")
            .ok()
            .and_then(|v| v.extract::<bool>().ok())
            .unwrap_or(false)
        {
            return Ok(py.None());
        }
        Ok(node.getattr("function")?.unbind())
    }

    /// The class containing the test, or None for functions/doctests. A
    /// request scoped wider than class (module/package/session) has no single
    /// class, so pytest raises AttributeError.
    #[getter]
    fn cls(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        if self.scope > crate::fixture::Scope::Class {
            return Err(pyo3::exceptions::PyAttributeError::new_err(format!(
                "cls not available in {}-scoped context",
                self.scope.as_str()
            )));
        }
        Ok(crate::runner::current_resolve_instance(py)
            .map(|instance| instance.bind(py).get_type().unbind().into_any())
            .unwrap_or_else(|| py.None()))
    }

    /// The test instance the item runs on (a fresh Test class instance, or
    /// the unittest.TestCase instance), or None for plain functions.
    #[getter]
    fn instance(&self, py: Python<'_>) -> Py<PyAny> {
        crate::runner::current_resolve_instance(py).unwrap_or_else(|| py.None())
    }

    /// The module containing the test, or None for doctests. A session-scoped
    /// request has no single item, so pytest raises AttributeError here.
    #[getter]
    fn module(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        if self.scope == crate::fixture::Scope::Session {
            return Err(pyo3::exceptions::PyAttributeError::new_err(
                "module not available in session-scoped context",
            ));
        }
        Ok(py.None())
    }

    /// The filesystem path of the test's module. As with `module`, a
    /// session-scoped request has no single item and pytest raises here.
    #[getter]
    fn path(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        if self.scope == crate::fixture::Scope::Session {
            return Err(pyo3::exceptions::PyAttributeError::new_err(
                "path not available in session-scoped context",
            ));
        }
        Ok(self.node.bind(py).getattr("path")?.unbind())
    }

    /// Dynamically resolve (and cache) a fixture by name, like pytest's
    /// request.getfixturevalue. Delegates to the runner's per-item context.
    ///
    /// pytest-bdd injects step fixtures and target_fixtures into this
    /// request's FixtureManager view rather than the Rust registry; consult
    /// it first so those names resolve (a pinned `cached_result` value, or an
    /// alias to a collected step fixture via `registry_name`).
    fn getfixturevalue(slf: Bound<'_, Self>, argname: &str) -> PyResult<Py<PyAny>> {
        let py = slf.py();
        // pytest's `request` fixture resolves to the request itself
        // (pytest-bdd asks for it when a scenario function declares `request`).
        if argname == "request" {
            return Ok(slf.into_any().unbind());
        }
        let this = slf.borrow();
        if let Some(fm) = this.manager(py)
            && let Some(value) = Self::resolve_via_manager(py, fm.bind(py), argname)?
        {
            return Ok(value);
        }
        crate::runner::getfixturevalue(py, argname)
    }

    /// pytest-bdd's FixtureManager view. Built lazily from the running item's
    /// fixture registry and cached so injected defs persist for the request.
    #[getter]
    fn _fixturemanager(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        let mut guard = self.fixturemanager.lock().expect("request lock poisoned");
        if let Some(fm) = guard.as_ref() {
            return Ok(fm.clone_ref(py));
        }
        let fm = crate::runner::build_fixturemanager(py)?;
        *guard = Some(fm.clone_ref(py));
        Ok(fm)
    }

    /// The active (most-recently-registered) ShimFixtureDef for `name`, which
    /// pytest-bdd's inject_fixture pins a `cached_result` on. Builds the
    /// manager if needed.
    fn _get_active_fixturedef(&self, py: Python<'_>, name: &str) -> PyResult<Py<PyAny>> {
        let fm = self._fixturemanager(py)?;
        let defs = fm
            .bind(py)
            .getattr("_arg2fixturedefs")?
            .call_method1("get", (name,))?;
        if !defs.is_none()
            && let Ok(len) = defs.len()
            && len > 0
        {
            return Ok(defs.get_item(len - 1)?.unbind());
        }
        Err(pyo3::exceptions::PyKeyError::new_err(name.to_string()))
    }

    /// Apply a marker to the running item (pytest's request.applymarker);
    /// the engine re-evaluates xfail against dynamically added marks.
    fn applymarker(&self, py: Python<'_>, marker: Py<PyAny>) -> PyResult<()> {
        self.node.bind(py).call_method1("add_marker", (marker,))?;
        Ok(())
    }

    fn addfinalizer(&self, finalizer: Py<PyAny>) {
        self.finalizers
            .lock()
            .expect("request lock poisoned")
            .push(finalizer);
    }

    /// The session config proxy (pytest's `request.config`).
    #[getter]
    fn config(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        crate::python::existing_py_config(py)
            .ok_or_else(|| pyo3::exceptions::PyAttributeError::new_err("config"))
    }

    /// The test session proxy (pytest's `request.session`), exposing the
    /// `shouldstop`/`shouldfail` setters and `config` (a test setting
    /// `request.session.shouldstop = "..."` aborts and banners the reason).
    #[getter]
    fn session(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        let config = crate::python::existing_py_config(py)
            .ok_or_else(|| pyo3::exceptions::PyAttributeError::new_err("session"))?;
        Ok(py
            .import("pytest._node")?
            .getattr("_NodeSession")?
            .call1((config,))?
            .unbind())
    }
}
