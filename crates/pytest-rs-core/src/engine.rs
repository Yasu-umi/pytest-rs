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

        if let Err(err) = self.fire_collection_modifyitems(py) {
            eprintln!("INTERNAL ERROR: {}", python::format_exception(py, &err));
            return exit_code::INTERNAL_ERROR;
        }

        let n_items = self.session.items.len();
        if !self.config.quiet {
            println!(
                "collected {n_items} item{}\n",
                if n_items == 1 { "" } else { "s" }
            );
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
            println!(
                "{}",
                crate::runner::summary_line(
                    &self.session.reports,
                    python::warning_count(py),
                    started.elapsed()
                )
            );
            return exit_code::NO_TESTS_COLLECTED;
        }

        self.run_items(py);

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

        self.print_failures();
        if let Err(err) = self.print_plugin_summaries(py) {
            eprintln!("INTERNAL ERROR: {}", python::format_exception(py, &err));
        }
        let warning_count = python::warning_count(py);
        if warning_count > 0 && !self.config.quiet {
            println!("{}", center_banner("warnings summary"));
            for line in python::warning_summary_lines(py) {
                println!("{line}");
            }
        }
        println!(
            "{}",
            crate::runner::summary_line(&self.session.reports, warning_count, started.elapsed())
        );
        code
    }

    fn print_header(&self) {
        if self.config.quiet {
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
        println!("{}", center_banner("short test summary info"));
        for report in &failures {
            let phase = match report.phase {
                Phase::Call => "FAILED",
                _ => "ERROR",
            };
            println!("{phase} {}", report.nodeid);
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

    fn fire_collection_modifyitems(&mut self, py: Python<'_>) -> PyResult<()> {
        // Temporarily move items out so hooks can mutate the list while the
        // session stays borrowable.
        let mut items = std::mem::take(&mut self.session.items);
        let mut ctx = HookContext {
            py,
            session: &mut self.session,
            config: &self.config,
        };
        let mut result = Ok(());
        for plugin in &self.plugins {
            result = plugin.pytest_collection_modifyitems(&mut ctx, &mut items);
            if result.is_err() {
                break;
            }
        }
        self.session.items = items;
        result
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
        let files = crate::collect::collect_test_files(&rootdir, &self.config.paths)?;

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
            if let Err(err) =
                python::collect_conftest(py, &rootdir, conftest, &mut self.session.registry)
            {
                errors.push((conftest.clone(), python::format_exception(py, &err)));
            }
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
