use std::path::PathBuf;
use std::time::Instant;

use pyo3::prelude::*;

use crate::config::Config;
use crate::hooks::{HookContext, Plugin};
use crate::python;
use crate::report::{Outcome, Phase, exit_code};
use crate::session::Session;

pub struct Engine {
    pub plugins: Vec<Box<dyn Plugin>>,
    pub session: Session,
    pub config: Config,
}

impl Engine {
    pub fn new(plugins: Vec<Box<dyn Plugin>>, config: Config) -> Self {
        Self {
            plugins,
            session: Session::new(),
            config,
        }
    }

    /// Run the whole test session; returns the process exit code.
    pub fn run(&mut self, py: Python<'_>) -> i32 {
        let started = Instant::now();
        if let Err(err) = python::install_shim(py) {
            eprintln!("INTERNAL ERROR: failed to install pytest shim: {err}");
            return exit_code::INTERNAL_ERROR;
        }
        if let Err(err) = python::install_warning_capture(py, &self.config.w_options) {
            eprintln!("ERROR: invalid -W option: {}", err.value(py));
            return exit_code::USAGE_ERROR;
        }
        if self.config.get_flag("runxfail") {
            // --runxfail also neutralizes imperative pytest.xfail (pytest's
            // skipping plugin monkeypatches it the same way).
            let _ = py.run(
                c"import pytest\npytest.xfail = lambda reason='': None\n",
                None,
                None,
            );
        }

        if let Err(err) = self
            .fire_configure(py)
            .and_then(|()| self.fire_sessionstart(py))
        {
            if python::is_usage_error(py, &err) {
                eprintln!("ERROR: {}", err.value(py));
                return exit_code::USAGE_ERROR;
            }
            eprintln!("INTERNAL ERROR: {}", python::format_exception(py, &err));
            return exit_code::INTERNAL_ERROR;
        }

        // -n worker mode: this process is driven over stdin/stdout; it
        // never collects or reports on its own.
        if self.config.is_worker() {
            return self.run_worker(py);
        }

        self.print_header();

        let collect_errors = match self.collect(py) {
            Ok(errors) => errors,
            Err(message) => {
                eprintln!("ERROR: {message}");
                return exit_code::USAGE_ERROR;
            }
        };
        for (path, err) in &collect_errors {
            eprintln!("ERROR collecting {}", path.display());
            eprintln!("{err}");
        }
        if !collect_errors.is_empty() {
            return exit_code::INTERRUPTED;
        }

        if let Err(message) = self.check_strict_markers() {
            println!("{message}");
            return exit_code::USAGE_ERROR;
        }

        let collected = self.session.items.len();
        if let Err(err) = self.apply_selection(py) {
            if python::is_usage_error(py, &err) {
                eprintln!("ERROR: {}", err.value(py));
                return exit_code::USAGE_ERROR;
            }
            eprintln!("INTERNAL ERROR: {}", python::format_exception(py, &err));
            return exit_code::INTERNAL_ERROR;
        }
        if let Err(err) = self.fire_collection_modifyitems(py) {
            eprintln!("INTERNAL ERROR: {}", python::format_exception(py, &err));
            return exit_code::INTERNAL_ERROR;
        }

        let n_items = self.session.items.len();
        if !self.config.quiet && !self.config.no_terminal() {
            let deselected = collected - n_items;
            if deselected > 0 {
                println!(
                    "collected {collected} items / {deselected} deselected / {n_items} selected\n"
                );
            } else {
                println!(
                    "collected {n_items} item{}\n",
                    if n_items == 1 { "" } else { "s" }
                );
            }
        }

        if self.config.collect_only {
            for item in &self.session.items {
                println!("{}", item.nodeid);
            }
            return if n_items == 0 {
                exit_code::NO_TESTS_COLLECTED
            } else {
                exit_code::OK
            };
        }
        if n_items == 0 {
            if !self.config.no_terminal() {
                println!(
                    "{}",
                    crate::runner::summary_line(
                        &self.session.reports,
                        python::warning_count(py),
                        started.elapsed()
                    )
                );
            }
            return exit_code::NO_TESTS_COLLECTED;
        }

        match self.config.numprocesses() {
            Some(workers) => self.run_dist(py, workers),
            None => self.run_items(py),
        }

        let failed = self
            .session
            .reports
            .iter()
            .any(|r| r.outcome == Outcome::Failed);
        let mut code = if failed {
            exit_code::TESTS_FAILED
        } else {
            exit_code::OK
        };

        if let Err(err) = self.fire_sessionfinish(py, code) {
            eprintln!("INTERNAL ERROR: {}", python::format_exception(py, &err));
        }
        if let Some(forced) = self.session.exit_code_override {
            code = forced;
        }

        if self.config.no_terminal() {
            return code;
        }
        self.print_failures();
        if let Err(err) = self.print_plugin_summaries(py) {
            eprintln!("INTERNAL ERROR: {}", python::format_exception(py, &err));
        }
        let warning_count = python::warning_count(py) + self.session.worker_warning_count;
        if warning_count > 0 && !self.config.quiet {
            println!("{}", center_banner("warnings summary"));
            for line in python::warning_summary_lines(py) {
                println!("{line}");
            }
            for line in &self.session.worker_warnings {
                println!("{line}");
            }
        }
        if let Some(banner) = &self.session.dist_banner {
            println!("{}", center_banner(banner));
        }
        self.print_short_summary();
        println!(
            "{}",
            crate::runner::summary_line(&self.session.reports, warning_count, started.elapsed())
        );
        code
    }

    fn print_header(&self) {
        if self.config.quiet || self.config.no_terminal() {
            return;
        }
        println!("{}", center_banner("test session starts"));
        println!(
            "platform {} -- pytest-rs {}",
            std::env::consts::OS,
            env!("CARGO_PKG_VERSION"),
        );
        println!("rootdir: {}", self.config.rootdir.display());
    }

    fn print_failures(&self) {
        let failures: Vec<_> = self
            .session
            .reports
            .iter()
            .filter(|r| r.outcome == Outcome::Failed)
            .collect();
        if failures.is_empty() {
            return;
        }
        println!("\n{}", center_banner("FAILURES"));
        for report in &failures {
            let name = report.nodeid.rsplit("::").next().unwrap_or(&report.nodeid);
            println!("{}", center_named(name));
            if let Some(longrepr) = &report.longrepr {
                println!("{longrepr}");
            }
        }
    }

    /// The "short test summary info" section, controlled by -r chars
    /// (default fE: failures and errors).
    fn print_short_summary(&self) {
        let chars = self.config.get_value("report-chars").unwrap_or("fE");
        let chars = if chars.contains('a') {
            "fEsxX".to_string()
        } else if chars.contains('A') {
            "fEsxXp".to_string()
        } else if chars == "N" {
            String::new()
        } else {
            chars.to_string()
        };

        let mut lines = Vec::new();
        // Skips group by (location, reason): "SKIPPED [2] file.py:3: reason".
        let mut skip_groups: Vec<((String, String), usize)> = Vec::new();
        for report in &self.session.reports {
            let entry = match (report.phase, report.outcome) {
                (Phase::Call, Outcome::Failed) if chars.contains('f') => Some("FAILED"),
                (Phase::Setup | Phase::Teardown, Outcome::Failed) if chars.contains('E') => {
                    Some("ERROR")
                }
                (_, Outcome::Skipped) if chars.contains('s') => {
                    let location = report
                        .location
                        .clone()
                        .unwrap_or_else(|| report.nodeid.clone());
                    let reason = report.longrepr.clone().unwrap_or_default();
                    let key = (location, reason);
                    match skip_groups.iter_mut().find(|(group, _)| group == &key) {
                        Some((_, count)) => *count += 1,
                        None => skip_groups.push((key, 1)),
                    }
                    None
                }
                (_, Outcome::XFailed) if chars.contains('x') => Some("XFAIL"),
                (_, Outcome::XPassed) if chars.contains('X') => Some("XPASS"),
                (Phase::Call, Outcome::Passed) if chars.contains('p') => Some("PASSED"),
                _ => None,
            };
            if let Some(word) = entry {
                let mut line = format!("{word} {}", report.nodeid);
                if let Some(message) = report.longrepr.as_deref().and_then(short_message) {
                    line.push_str(&format!(" - {message}"));
                }
                lines.push(line);
            }
        }
        for ((location, reason), count) in skip_groups {
            lines.push(format!("SKIPPED [{count}] {location}: {reason}"));
        }
        if lines.is_empty() {
            return;
        }
        println!("{}", center_banner("short test summary info"));
        for line in lines {
            println!("{line}");
        }
    }

    fn print_plugin_summaries(&mut self, py: Python<'_>) -> PyResult<()> {
        let mut out = String::new();
        let mut ctx = HookContext {
            py,
            session: &mut self.session,
            config: &self.config,
        };
        for plugin in &self.plugins {
            plugin.pytest_terminal_summary(&mut ctx, &mut out)?;
        }
        if !out.is_empty() {
            println!("{out}");
        }
        Ok(())
    }

    // ---- hook dispatch -------------------------------------------------

    fn fire_configure(&mut self, py: Python<'_>) -> PyResult<()> {
        let mut ctx = HookContext {
            py,
            session: &mut self.session,
            config: &self.config,
        };
        for plugin in self.plugins.iter_mut() {
            plugin.pytest_configure(&mut ctx)?;
        }
        Ok(())
    }

    fn fire_sessionstart(&mut self, py: Python<'_>) -> PyResult<()> {
        let mut ctx = HookContext {
            py,
            session: &mut self.session,
            config: &self.config,
        };
        for plugin in self.plugins.iter_mut() {
            plugin.pytest_sessionstart(&mut ctx)?;
        }
        Ok(())
    }

    /// --strict-markers / --strict (CLI or ini): every mark must be
    /// registered in the `markers` ini option or be a builtin/bundled one.
    fn check_strict_markers(&self) -> Result<(), String> {
        let strict = self.config.get_flag("strict-markers")
            || self.config.get_flag("strict")
            || matches!(
                self.config.get_ini("strict_markers").map(str::trim),
                Some("true") | Some("True") | Some("1")
            )
            || matches!(
                self.config.get_ini("strict").map(str::trim),
                Some("true") | Some("True") | Some("1")
            );
        if !strict {
            return Ok(());
        }

        // Marks owned by the core or bundled plugins.
        const KNOWN: [&str; 12] = [
            "skip",
            "skipif",
            "xfail",
            "parametrize",
            "usefixtures",
            "filterwarnings",
            "tryfirst",
            "trylast",
            "asyncio",
            "benchmark",
            "no_cover",
            "xdist_group",
        ];
        let registered: std::collections::HashSet<String> = self
            .config
            .get_ini("markers")
            .map(|lines| {
                lines
                    .lines()
                    .filter_map(|line| {
                        let name = line.trim().split([':', '(']).next()?.trim();
                        (!name.is_empty()).then(|| name.to_string())
                    })
                    .collect()
            })
            .unwrap_or_default();

        for item in &self.session.items {
            for mark in &item.marks {
                if !KNOWN.contains(&mark.name.as_str()) && !registered.contains(&mark.name) {
                    return Err(format!(
                        "'{}' not found in `markers` configuration option",
                        mark.name
                    ));
                }
            }
        }
        Ok(())
    }

    /// -m / -k deselection (pytest applies these before plugin
    /// collection_modifyitems hooks).
    fn apply_selection(&mut self, py: Python<'_>) -> PyResult<()> {
        if let Some(expr) = self.config.get_value("markexpr").map(str::to_string) {
            let expr = expr.trim().to_string();
            let mut error = None;
            self.session.items.retain(|item| {
                match crate::markexpr::evaluate(&expr, |name| {
                    item.marks.iter().any(|mark| mark.name == name)
                }) {
                    Ok(keep) => keep,
                    Err(message) => {
                        error.get_or_insert(message);
                        true
                    }
                }
            });
            if let Some(message) = error {
                return Err(python::usage_error(
                    py,
                    &format!("Wrong expression passed to '-m': {expr}: {message}"),
                ));
            }
        }
        if let Some(expr) = self.config.get_value("keyword").map(str::to_string) {
            let expr = expr.trim().to_string();
            let mut error = None;
            self.session.items.retain(|item| {
                // -k matches case-insensitively against the test name part.
                let name = item.nodeid.split_once("::").map_or("", |(_, n)| n);
                let haystack = name.to_lowercase();
                match crate::markexpr::evaluate(&expr, |token| {
                    haystack.contains(&token.to_lowercase())
                }) {
                    Ok(keep) => keep,
                    Err(message) => {
                        error.get_or_insert(message);
                        true
                    }
                }
            });
            if let Some(message) = error {
                return Err(python::usage_error(
                    py,
                    &format!("Wrong expression passed to '-k': {expr}: {message}"),
                ));
            }
        }
        Ok(())
    }

    fn fire_collection_modifyitems(&mut self, py: Python<'_>) -> PyResult<()> {
        // Temporarily move items out so hooks can mutate the list while the
        // session stays borrowable.
        let mut items = std::mem::take(&mut self.session.items);
        {
            let mut ctx = HookContext {
                py,
                session: &mut self.session,
                config: &self.config,
            };
            for plugin in &self.plugins {
                if let Err(err) = plugin.pytest_collection_modifyitems(&mut ctx, &mut items) {
                    self.session.items = items;
                    return Err(err);
                }
            }
        }
        let result = self.run_py_modifyitems(py, &mut items);
        self.session.items = items;
        result
    }

    /// conftest pytest_collection_modifyitems hooks: items are exposed as
    /// node proxies; reordering, deselection, and added markers are read
    /// back from the proxy list.
    fn run_py_modifyitems(
        &mut self,
        py: Python<'_>,
        items: &mut Vec<crate::collect::TestItem>,
    ) -> PyResult<()> {
        let hook_funcs: Vec<Py<pyo3::PyAny>> = self
            .session
            .py_hooks
            .iter()
            .filter(|hook| hook.name == "pytest_collection_modifyitems")
            .map(|hook| hook.func.clone_ref(py))
            .collect();
        if hook_funcs.is_empty() {
            return Ok(());
        }

        let config_proxy = python::make_py_config(py, &self.config)?;
        let nodes: Vec<Py<pyo3::PyAny>> = items
            .iter()
            .map(|item| python::make_node(py, item))
            .collect::<PyResult<_>>()?;
        let node_list = pyo3::types::PyList::new(py, nodes.iter().map(|n| n.bind(py)))?;

        for func in &hook_funcs {
            python::call_py_hook(
                py,
                func,
                &[
                    ("config", config_proxy.clone_ref(py)),
                    ("items", node_list.clone().unbind().into_any()),
                    ("session", py.None()),
                ],
            )?;
        }

        // Read back order/membership (by nodeid) and any added markers.
        let mut by_nodeid: std::collections::HashMap<String, crate::collect::TestItem> =
            std::mem::take(items)
                .into_iter()
                .map(|item| (item.nodeid.clone(), item))
                .collect();
        for node in node_list.iter() {
            let nodeid: String = node.getattr("nodeid")?.extract()?;
            if let Some(mut item) = by_nodeid.remove(&nodeid) {
                let mut marks = Vec::new();
                for mark in node.getattr("own_markers")?.try_iter()? {
                    let mark = mark?;
                    marks.push(crate::collect::MarkData {
                        name: mark.getattr("name")?.extract()?,
                        obj: mark.unbind(),
                    });
                }
                item.marks = marks;
                items.push(item);
            }
        }
        Ok(())
    }

    /// Fire conftest hooks that only take `config` (e.g. pytest_configure).
    fn fire_py_hooks_simple(&mut self, py: Python<'_>, name: &str) -> PyResult<()> {
        let hook_funcs: Vec<Py<pyo3::PyAny>> = self
            .session
            .py_hooks
            .iter()
            .filter(|hook| hook.name == name)
            .map(|hook| hook.func.clone_ref(py))
            .collect();
        if hook_funcs.is_empty() {
            return Ok(());
        }
        let config_proxy = python::make_py_config(py, &self.config)?;
        for func in &hook_funcs {
            python::call_py_hook(py, func, &[("config", config_proxy.clone_ref(py))])?;
        }
        Ok(())
    }

    fn fire_sessionfinish(&mut self, py: Python<'_>, code: i32) -> PyResult<()> {
        let mut ctx = HookContext {
            py,
            session: &mut self.session,
            config: &self.config,
        };
        for plugin in self.plugins.iter_mut() {
            plugin.pytest_sessionfinish(&mut ctx, code)?;
        }
        Ok(())
    }

    // ---- collection ----------------------------------------------------

    /// Returns per-file collection errors (formatted).
    fn collect(&mut self, py: Python<'_>) -> Result<Vec<(PathBuf, String)>, String> {
        let rootdir = self.config.rootdir.clone();
        // Relative CLI paths (and bare collection) resolve against the
        // invocation dir; rootdir only anchors node ids.
        let files =
            crate::collect::collect_test_files(&self.config.invocation_dir, &self.config.paths)?;

        let mut conftests: Vec<PathBuf> = Vec::new();
        for file in &files {
            let mut dir = file.parent();
            let mut chain = Vec::new();
            while let Some(d) = dir {
                let conftest = d.join("conftest.py");
                if conftest.exists() {
                    chain.push(conftest);
                }
                if d == rootdir {
                    break;
                }
                dir = d.parent();
            }
            chain.reverse();
            for conftest in chain {
                if !conftests.contains(&conftest) {
                    conftests.push(conftest);
                }
            }
        }

        let mut errors = Vec::new();
        if let Err(err) = python::register_builtin_fixtures(py, &mut self.session.registry) {
            return Err(python::format_exception(py, &err));
        }
        for conftest in &conftests {
            if let Err(err) = python::collect_conftest(
                py,
                &rootdir,
                conftest,
                &mut self.session.registry,
                &mut self.session.py_hooks,
            ) {
                errors.push((conftest.clone(), python::format_exception(py, &err)));
            }
        }

        // conftest pytest_configure hooks run once conftests are loaded.
        if let Err(err) = self.fire_py_hooks_simple(py, "pytest_configure") {
            errors.push((rootdir.clone(), python::format_exception(py, &err)));
        }
        for file in &files {
            if let Err(err) = python::collect_module(
                py,
                &rootdir,
                file,
                &mut self.session.items,
                &mut self.session.registry,
            ) {
                errors.push((file.clone(), python::format_exception(py, &err)));
            }
        }

        // Expand items over parametrized fixtures in their closure.
        let items = std::mem::take(&mut self.session.items);
        match python::expand_fixture_params(py, items, &self.session.registry) {
            Ok(expanded) => self.session.items = expanded,
            Err(err) => return Err(python::format_exception(py, &err)),
        }
        Ok(errors)
    }
}

/// The one-line summary appended to FAILED/ERROR entries: the first
/// E-prefixed explanation line, else the exception line.
fn short_message(longrepr: &str) -> Option<String> {
    let from_e_line = longrepr.lines().find_map(|line| {
        line.strip_prefix("E ")
            .map(|rest| rest.trim_start().to_string())
    });
    from_e_line
        .or_else(|| {
            longrepr
                .lines()
                .rev()
                .find(|line| !line.trim().is_empty())
                .map(|line| line.trim().to_string())
        })
        .filter(|message| !message.is_empty())
}

pub fn center_banner(label: &str) -> String {
    center_with(label, '=')
}

fn center_named(label: &str) -> String {
    center_with(label, '_')
}

pub fn center_with(label: &str, fill: char) -> String {
    const WIDTH: usize = 80;
    let label = format!(" {label} ");
    let pad = WIDTH.saturating_sub(label.len());
    let left = pad / 2;
    let right = pad - left;
    format!(
        "{}{}{}",
        fill.to_string().repeat(left),
        label,
        fill.to_string().repeat(right)
    )
}
