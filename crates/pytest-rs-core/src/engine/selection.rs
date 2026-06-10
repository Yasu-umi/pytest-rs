//! --deselect / -k / -m selection and --strict-markers validation.

#[allow(unused_imports)]
use super::*;
use crate::python;

impl Engine {
    /// --strict-markers / --strict via CLI or ini.
    pub(crate) fn strict_markers(&self) -> bool {
        self.config.get_flag("strict-markers")
            || self.config.get_flag("strict")
            || matches!(
                self.config.get_ini("strict_markers").map(str::trim),
                Some("true") | Some("True") | Some("1")
            )
            || matches!(
                self.config.get_ini("strict").map(str::trim),
                Some("true") | Some("True") | Some("1")
            )
    }

    /// strict_parametrization_ids ini (falling back to strict): duplicate
    /// parametrization IDs become a collection error instead of suffixing.
    pub(crate) fn strict_parametrization_ids(&self) -> bool {
        let enabled = |value: &str| matches!(value.trim(), "true" | "True" | "1");
        match self.config.get_ini("strict_parametrization_ids") {
            Some(value) => enabled(value),
            None => self.config.get_ini("strict").is_some_and(enabled),
        }
    }

    /// --strict-markers / --strict (CLI or ini): every mark must be
    /// registered in the `markers` ini option or be a builtin/bundled one.
    pub(crate) fn check_strict_markers(&self, py: Python<'_>) -> Result<(), String> {
        if !self.strict_markers() {
            return Ok(());
        }

        // Read through the config proxy: plugins register their markers at
        // configure time via addinivalue_line("markers", ...), which lands
        // there, not in the Rust-side ini snapshot.
        let proxy_lines: Vec<String> = python::existing_py_config(py)
            .and_then(|proxy| {
                let value = proxy.bind(py).call_method1("getini", ("markers",)).ok()?;
                value.extract::<Vec<String>>().ok().or_else(|| {
                    value
                        .extract::<Option<String>>()
                        .ok()
                        .flatten()
                        .map(|raw| raw.lines().map(str::to_string).collect())
                })
            })
            .unwrap_or_default();
        let ini_lines: Vec<String> = self
            .config
            .get_ini_lines("markers")
            .into_iter()
            .map(str::to_string)
            .collect();
        let registered: std::collections::HashSet<String> = proxy_lines
            .iter()
            .chain(ini_lines.iter())
            .filter_map(|line| {
                let name = line.trim().split([':', '(']).next()?.trim();
                (!name.is_empty()).then(|| name.to_string())
            })
            .collect();

        for item in &self.session.items {
            for mark in &item.marks {
                if !BUILTIN_MARKS.contains(&mark.name.as_str()) && !registered.contains(&mark.name)
                {
                    return Err(format!(
                        "'{}' not found in `markers` configuration option",
                        mark.name
                    ));
                }
            }
        }
        Ok(())
    }

    /// --deselect runs before the modifyitems hooks (upstream main.py's
    /// hookimpl is not trylast like the -m/-k one).
    pub(crate) fn apply_deselect(&mut self) -> Result<(), String> {
        if let Some(prefixes) = self.config.get_values("deselect") {
            let prefixes: Vec<String> = prefixes.iter().map(|s| s.to_string()).collect();
            // pytest matches by plain nodeid prefix (main.py).
            let (kept, removed): (Vec<_>, Vec<_>) =
                self.session.items.drain(..).partition(|item| {
                    !prefixes.iter().any(|p| item.nodeid.starts_with(p.as_str()))
                });
            self.session.items = kept;
            self.session.deselected_items.extend(removed);
        }
        Ok(())
    }

    /// -m / -k deselection (upstream's trylast collection_modifyitems
    /// hookimpl: runs after conftest/plugin hooks).
    pub(crate) fn apply_selection(&mut self, py: Python<'_>) -> PyResult<()> {
        // -k runs before -m, like upstream pytest_collection_modifyitems.
        if let Some(expr) = self.config.get_value("keyword").map(str::to_string) {
            let expr = expr.trim_start().to_string();
            if !expr.is_empty() {
                let shim = py.import("pytest._expression")?;
                // Compile errors surface as UsageError with upstream wording.
                let compiled = shim.call_method1("compile_for_engine", (expr.as_str(), "-k"))?;
                let mut error: Option<PyErr> = None;
                let (kept, removed): (Vec<_>, Vec<_>) =
                    self.session.items.drain(..).partition(|item| {
                        let names = python::keyword_match_names(py, item);
                        match shim
                            .call_method1("evaluate_keywords", (&compiled, names))
                            .and_then(|value| value.extract::<bool>())
                        {
                            Ok(keep) => keep,
                            Err(err) => {
                                error.get_or_insert(err);
                                true
                            }
                        }
                    });
                self.session.items = kept;
                self.session.deselected_items.extend(removed);
                if let Some(err) = error {
                    return Err(err);
                }
            }
        }
        if let Some(expr) = self.config.get_value("markexpr").map(str::to_string) {
            let expr = expr.trim().to_string();
            if !expr.is_empty() {
                let shim = py.import("pytest._expression")?;
                let compiled = shim.call_method1("compile_for_engine", (expr.as_str(), "-m"))?;
                let mut error: Option<PyErr> = None;
                let (kept, removed): (Vec<_>, Vec<_>) =
                    self.session.items.drain(..).partition(|item| {
                        let marks: Vec<Py<PyAny>> = item
                            .marks
                            .iter()
                            .map(|mark| mark.obj.clone_ref(py))
                            .collect();
                        match shim
                            .call_method1("evaluate_marks", (&compiled, marks))
                            .and_then(|value| value.extract::<bool>())
                        {
                            Ok(keep) => keep,
                            Err(err) => {
                                error.get_or_insert(err);
                                true
                            }
                        }
                    });
                self.session.items = kept;
                self.session.deselected_items.extend(removed);
                if let Some(err) = error {
                    return Err(err);
                }
            }
        }
        Ok(())
    }

    /// Resolve -n: "auto"/"logical" go through conftest
    /// pytest_xdist_auto_num_workers hooks (firstresult, LIFO like pluggy),
    /// falling back to upstream xdist's default detection (the
    /// PYTEST_XDIST_AUTO_NUM_WORKERS env override, psutil if installed —
    /// the pytest-xdist[psutil] extra: physical cores for auto — then
    /// sched_getaffinity/cpu_count). --maxprocesses caps auto/logical
    /// only, like upstream.
    #[cfg(feature = "xdist")]
    pub(crate) fn resolve_numprocesses(&mut self, py: Python<'_>) -> Option<usize> {
        // -d / --tx gateway specs without -n: one worker per expanded spec.
        if self.config.numprocesses_spec().is_none()
            && (self.config.get_flag("dist-load") || self.config.get_value("tx").is_some())
            && let Some(workers) = self.config.tx_worker_chdirs()
        {
            return (!workers.is_empty()).then_some(workers.len());
        }
        let value = self.config.numprocesses_spec()?.to_string();
        let n = match value.as_str() {
            "auto" | "logical" => {
                let hook_funcs: Vec<Py<pyo3::PyAny>> = self
                    .session
                    .py_hooks
                    .iter()
                    .filter(|hook| hook.name == "pytest_xdist_auto_num_workers")
                    .map(|hook| hook.func.clone_ref(py))
                    .collect();
                let from_hook = hook_funcs.iter().rev().find_map(|func| {
                    let config = python::make_py_config(py, &self.config).ok()?;
                    python::call_py_hook(py, func, &[("config", config)])
                        .ok()
                        .and_then(|res| res.bind(py).extract::<usize>().ok())
                });
                let n = from_hook
                    .unwrap_or_else(|| python::xdist_auto_num_workers(py, value == "logical"));
                match self.config.maxprocesses() {
                    Some(max) => n.min(max),
                    None => n,
                }
            }
            other => other.parse().ok()?,
        };
        (n > 0).then_some(n)
    }
}
