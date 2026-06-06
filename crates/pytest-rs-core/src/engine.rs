use std::path::PathBuf;
use std::time::Instant;

use pyo3::prelude::*;

use crate::config::Config;
use crate::hooks::{HookContext, Plugin};
use crate::python;
use crate::report::{Outcome, Phase, exit_code};
use crate::session::Session;

/// Marks owned by the core or bundled plugins.
pub(crate) const BUILTIN_MARKS: [&str; 12] = [
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

pub struct Engine {
    pub plugins: Vec<Box<dyn Plugin>>,
    pub session: Session,
    pub config: Config,
    /// cacheprovider state (--lf/--ff/--nf, lastfailed persistence).
    cache: Option<crate::cache::CacheState>,
}

impl Engine {
    pub fn new(plugins: Vec<Box<dyn Plugin>>, config: Config) -> Self {
        Self {
            plugins,
            session: Session::new(),
            config,
            cache: None,
        }
    }

    /// Run the whole test session; returns the process exit code.
    pub fn run(&mut self, py: Python<'_>) -> i32 {
        let started = Instant::now();
        if let Err(err) = python::activate_virtualenv(py) {
            eprintln!("INTERNAL ERROR: failed to activate virtualenv: {err}");
            return exit_code::INTERNAL_ERROR;
        }
        if let Err(err) = python::install_shim(py) {
            eprintln!("INTERNAL ERROR: failed to install pytest shim: {err}");
            return exit_code::INTERNAL_ERROR;
        }
        if let Some(report) = self.config.get_value("doctest-report") {
            const CHOICES: &[&str] = &["none", "cdiff", "udiff", "ndiff", "only_first_failure"];
            if !CHOICES.iter().any(|c| c.eq_ignore_ascii_case(report)) {
                eprintln!(
                    "error: argument --doctest-report: invalid choice: '{report}' (choose from {})",
                    CHOICES.join(", ")
                );
                return exit_code::USAGE_ERROR;
            }
        }
        let ini_filters: Vec<String> = self
            .config
            .get_ini("filterwarnings")
            .map(|lines| {
                lines
                    .lines()
                    .map(str::trim)
                    .filter(|line| !line.is_empty())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        if let Err(err) = python::install_warning_capture(py, &ini_filters, &self.config.w_options)
        {
            eprintln!("ERROR: {}", err.value(py));
            return exit_code::USAGE_ERROR;
        }
        if let Some(mode) = self
            .config
            .get_value("log-file-mode")
            .or_else(|| self.config.get_ini("log_file_mode"))
            && !matches!(mode, "w" | "a")
        {
            eprintln!(
                "error: argument --log-file-mode: invalid choice: '{mode}' (choose from 'w', 'a')"
            );
            return exit_code::USAGE_ERROR;
        }
        // Session-wide logging handlers: log_file writes, log_cli interleaves
        // live records with the progress output.
        self.session.live_logging = python::configure_logging(py, &self.config);

        // Global output capture: -s / --capture=no disable, default "fd"
        // (dup2-based, so os.write and C-level output are captured too).
        let capture_mode = if self.config.get_flag("capture-disable") {
            "no"
        } else {
            self.config.get_value("capture").unwrap_or("fd")
        };
        if !matches!(capture_mode, "fd" | "sys" | "no" | "tee-sys") {
            eprintln!(
                "error: argument --capture: invalid choice: '{capture_mode}' (choose from 'fd', 'sys', 'no', 'tee-sys')"
            );
            return exit_code::USAGE_ERROR;
        }
        python::configure_capture(py, capture_mode);

        // --junitxml: arm the XML writer (workers never write; the parent
        // streams every report through it at session end).
        if let Some(path) = self.config.get_value("junit-xml").map(str::to_string)
            && !self.config.is_worker()
        {
            if std::path::Path::new(&path).is_dir() {
                eprintln!("ERROR: --junitxml must be a filename, given: {path}");
                return exit_code::USAGE_ERROR;
            }
            if let Err(err) = python::junit_configure(py, &self.config, &path) {
                eprintln!("INTERNAL ERROR: {}", python::format_exception(py, &err));
                return exit_code::INTERNAL_ERROR;
            }
        }

        // Arm unknown-mark validation (PytestUnknownMarkWarning on access).
        if let Err(err) = python::configure_mark_generator(
            py,
            &self.config,
            self.strict_markers(),
            self.strict_parametrization_ids(),
        ) {
            eprintln!("INTERNAL ERROR: {}", python::format_exception(py, &err));
            return exit_code::INTERNAL_ERROR;
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
        #[cfg(feature = "xdist")]
        if self.config.is_worker() {
            return self.run_worker(py);
        }

        self.print_header();
        self.cache = Some(crate::cache::CacheState::new(py, &self.config));

        // --cache-show: display cache contents instead of running tests.
        if let Some(glob) = self.config.get_value("cache-show").map(str::to_string) {
            let glob = if glob.is_empty() { "*" } else { &glob };
            return match python::cache_show(py, &self.config, glob) {
                Ok(()) => exit_code::OK,
                Err(err) => {
                    eprintln!("INTERNAL ERROR: {}", python::format_exception(py, &err));
                    exit_code::INTERNAL_ERROR
                }
            };
        }

        if self.session.live_logging {
            python::log_set_live_when(py, "collection");
        }
        let collect_errors = match self.collect(py) {
            Ok(errors) => errors,
            Err(message) => {
                eprintln!("ERROR: {message}");
                return exit_code::USAGE_ERROR;
            }
        };
        let n_collect_errors = collect_errors.len();
        if n_collect_errors > 0 {
            // Collection errors still report as errors in the summary.
            for (path, err) in collect_errors {
                let nodeid = crate::collect::file_nodeid(&self.config.rootdir, &path);
                self.session.collect_errors.push((nodeid.clone(), err.clone()));
                self.session.reports.push(crate::report::TestReport {
                    nodeid,
                    phase: Phase::Setup,
                    outcome: Outcome::Failed,
                    duration: std::time::Duration::ZERO,
                    longrepr: Some(err),
                    location: None,
                    subtest_desc: None,
                    sections: Vec::new(),
                });
            }
            // --maxfail aborting collection exits TESTS_FAILED with a
            // "stopping after N failures" banner; otherwise INTERRUPTED.
            let maxfail_hit = self
                .config
                .maxfail()
                .is_some_and(|m| n_collect_errors >= m);
            if !self.config.get_flag("continue-on-collection-errors") || maxfail_hit {
                if !self.config.no_terminal() {
                    if !self.config.quiet {
                        let n_items = self.session.items.len();
                        println!(
                            "collected {n_items} item{} / {n_collect_errors} error{}",
                            if n_items == 1 { "" } else { "s" },
                            if n_collect_errors == 1 { "" } else { "s" }
                        );
                    }
                    self.print_collect_errors();
                }
                // A file that fails collection is a "last failed" entry.
                if let Some(cache) = &self.cache {
                    cache.sessionfinish(py, &self.config, &self.session.reports);
                }
                self.write_junit_xml(py);
                if !self.config.no_terminal() {
                    self.print_short_summary();
                    let banner = if maxfail_hit {
                        format!("stopping after {n_collect_errors} failures")
                    } else {
                        format!(
                            "Interrupted: {n_collect_errors} error{} during collection",
                            if n_collect_errors == 1 { "" } else { "s" }
                        )
                    };
                    println!("{}", center_with(&banner, '!'));
                    println!(
                        "{}",
                        crate::runner::summary_line(
                            &self.session.reports,
                            self.session.deselected,
                            python::warning_count(py),
                            started.elapsed()
                        )
                    );
                }
                return if maxfail_hit {
                    exit_code::TESTS_FAILED
                } else {
                    exit_code::INTERRUPTED
                };
            }
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
        if let Some(cache) = &mut self.cache {
            cache.modify_items(
                &self.config,
                &mut self.session.items,
                &mut self.session.deselected_items,
            );
        }
        if let Err(err) = self.fire_py_deselected(py) {
            eprintln!("INTERNAL ERROR: {}", python::format_exception(py, &err));
            return exit_code::INTERNAL_ERROR;
        }

        let n_items = self.session.items.len();
        // Plugins may also expand items (e.g. loop-factory
        // parametrization), so saturate against growth.
        self.session.deselected = collected.saturating_sub(n_items);
        if !self.config.quiet && !self.config.no_terminal() {
            let deselected = self.session.deselected;
            if deselected > 0 {
                println!(
                    "collected {collected} items / {deselected} deselected / {n_items} selected"
                );
            } else if n_collect_errors > 0 {
                println!(
                    "collected {n_items} item{} / {n_collect_errors} error{}",
                    if n_items == 1 { "" } else { "s" },
                    if n_collect_errors == 1 { "" } else { "s" }
                );
            } else {
                println!(
                    "collected {n_items} item{}",
                    if n_items == 1 { "" } else { "s" }
                );
            }
            if let Some(line) = self
                .cache
                .as_ref()
                .and_then(|cache| cache.status_line(&self.config))
            {
                println!("{line}");
            }
            println!();
        }

        if self.config.collect_only {
            if !self.config.no_terminal() {
                if self.config.quiet_level >= 2 {
                    // -qq: per-file counts ("test_x.py: 3").
                    let mut counts: Vec<(String, usize)> = Vec::new();
                    for item in &self.session.items {
                        let file = item.nodeid.split("::").next().unwrap_or("").to_string();
                        match counts.iter_mut().find(|(name, _)| name == &file) {
                            Some((_, count)) => *count += 1,
                            None => counts.push((file, 1)),
                        }
                    }
                    for (file, count) in counts {
                        println!("{file}: {count}");
                    }
                } else if self.config.quiet {
                    for item in &self.session.items {
                        println!("{}", item.nodeid);
                    }
                } else {
                    self.print_collect_tree();
                }
                self.print_collect_only_summary(started.elapsed());
            }
            return if n_items == 0 {
                exit_code::NO_TESTS_COLLECTED
            } else {
                exit_code::OK
            };
        }
        if n_items == 0 {
            // Module-level skips count as a run that skipped everything
            // (exit 0), not as "no tests collected".
            let code = if self
                .session
                .reports
                .iter()
                .any(|r| r.outcome == Outcome::Skipped)
            {
                exit_code::OK
            } else {
                exit_code::NO_TESTS_COLLECTED
            };
            if let Err(err) = self.fire_py_sessionfinish(py, code) {
                eprintln!("INTERNAL ERROR: {}", python::format_exception(py, &err));
            }
            // Stop the session-wide capture (errors surface on stderr).
            python::capture_session_end(py);
            if let Some(cache) = &self.cache {
                cache.sessionfinish(py, &self.config, &self.session.reports);
            }
            if self.config.no_terminal() {
                self.write_junit_xml(py);
            } else {
                self.print_warnings_summary(py);
                self.write_junit_xml(py);
                self.print_short_summary();
                println!(
                    "{}",
                    crate::runner::summary_line(
                        &self.session.reports,
                        self.session.deselected,
                        python::warning_count(py),
                        started.elapsed()
                    )
                );
            }
            return code;
        }

        #[cfg(feature = "xdist")]
        match self.config.numprocesses() {
            Some(workers) => self.run_dist(py, workers),
            None => self.run_items(py),
        }
        #[cfg(not(feature = "xdist"))]
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
        // Stop the session-wide capture (errors surface on stderr).
        python::capture_session_end(py);
        if let Some(cache) = &self.cache {
            cache.sessionfinish(py, &self.config, &self.session.reports);
        }
        if let Some(forced) = self.session.exit_code_override {
            code = forced;
        }

        if self.config.no_terminal() {
            self.write_junit_xml(py);
            return code;
        }
        if let Some(banner) = &self.session.abort_banner {
            println!("{}", center_with(banner, '!'));
        }
        // --continue-on-collection-errors: the ERRORS section was deferred
        // until after the run, like pytest's terminal reporter.
        self.print_collect_errors();
        self.print_failures();
        if let Err(err) = self.print_plugin_summaries(py) {
            eprintln!("INTERNAL ERROR: {}", python::format_exception(py, &err));
        }
        self.print_warnings_summary(py);
        self.print_passes();
        self.write_junit_xml(py);
        if let Some(banner) = &self.session.dist_banner {
            println!("{}", center_banner(banner));
        }
        self.print_short_summary();
        if let Some(n) = self.session.stopped_after {
            println!("{}", center_with(&format!("stopping after {n} failures"), '!'));
        }
        let warning_count = python::warning_count(py) + self.session.worker_warning_count;
        println!(
            "\n{}",
            crate::runner::summary_line(
                &self.session.reports,
                self.session.deselected,
                warning_count,
                started.elapsed(),
            )
        );
        code
    }

    /// The --collect-only hierarchy: <Dir>/<Package>/<Module>/<Class>/
    /// <Function> nodes, two-space indent per level.
    fn print_collect_tree(&self) {
        if self.session.items.is_empty() {
            return;
        }
        let root_name = self
            .config
            .rootdir
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_default();
        println!("<Dir {root_name}>");
        // The open chain of (label) nodes above the current item.
        let mut open: Vec<String> = Vec::new();
        for item in &self.session.items {
            let (file_part, rest) = match item.nodeid.split_once("::") {
                Some(parts) => parts,
                None => continue,
            };
            let mut labels: Vec<String> = Vec::new();
            let mut dir_so_far = self.config.rootdir.clone();
            let segments: Vec<&str> = file_part.split('/').collect();
            for dir in &segments[..segments.len().saturating_sub(1)] {
                dir_so_far = dir_so_far.join(dir);
                let kind = if dir_so_far.join("__init__.py").is_file() {
                    "Package"
                } else {
                    "Dir"
                };
                labels.push(format!("<{kind} {dir}>"));
            }
            if let Some(module) = segments.last() {
                labels.push(format!("<Module {module}>"));
            }
            let parts: Vec<&str> = rest.split("::").collect();
            for class in &parts[..parts.len().saturating_sub(1)] {
                labels.push(format!("<Class {class}>"));
            }
            if let Some(function) = parts.last() {
                labels.push(format!("<Function {function}>"));
            }
            // Print only the suffix that differs from the previous item.
            let shared = open
                .iter()
                .zip(labels.iter())
                .take_while(|(open_label, label)| open_label == label)
                .count();
            for (depth, label) in labels.iter().enumerate().skip(shared) {
                println!("{}{label}", "  ".repeat(depth + 1));
            }
            open = labels;
        }
    }

    /// The --collect-only closing banner ("N/M tests collected ...").
    fn print_collect_only_summary(&self, elapsed: std::time::Duration) {
        let selected = self.session.items.len();
        let deselected = self.session.deselected;
        let body = if selected == 0 && deselected == 0 {
            format!("no tests collected in {:.2}s", elapsed.as_secs_f64())
        } else if deselected > 0 {
            format!(
                "{selected}/{} tests collected ({deselected} deselected) in {:.2}s",
                selected + deselected,
                elapsed.as_secs_f64()
            )
        } else {
            format!(
                "{selected} test{} collected in {:.2}s",
                if selected == 1 { "" } else { "s" },
                elapsed.as_secs_f64()
            )
        };
        let color = if selected == 0 && deselected == 0 {
            "\x1b[33m" // yellow
        } else {
            "\x1b[32m" // green
        };
        println!();
        if self.config.quiet {
            // -q: the bare summary line, no banner.
            println!("{color}{body}\x1b[0m");
        } else {
            println!("{color}{}\x1b[0m", center_banner(&body));
        }
    }

    fn print_warnings_summary(&self, py: Python<'_>) {
        let warning_count = python::warning_count(py) + self.session.worker_warning_count;
        if warning_count == 0 || self.config.quiet {
            return;
        }
        println!("{}", center_banner("warnings summary"));
        for line in python::warning_summary_lines(py) {
            println!("{line}");
        }
        for line in &self.session.worker_warnings {
            println!("{line}");
        }
        println!("-- Docs: https://docs.pytest.org/en/stable/how-to/capture-warnings.html");
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
        // pytest shows the cachedir only when it is non-default or -v.
        let default_dir = std::env::var("TOX_ENV_DIR")
            .map(|tox| format!("{tox}/.pytest_cache"))
            .unwrap_or_else(|_| ".pytest_cache".to_string());
        let cache_dir = self
            .config
            .get_ini("cache_dir")
            .map(str::trim)
            .map(str::to_string)
            .unwrap_or(default_dir);
        if !self.config.plugin_disabled("cacheprovider")
            && (self.config.verbose > 0 || cache_dir != ".pytest_cache")
        {
            println!("cachedir: {cache_dir}");
        }
        println!("rootdir: {}", self.config.rootdir.display());
    }

    /// The ERRORS section: "ERROR collecting <file>" banners per collection
    /// error, plus "ERROR at setup/teardown of <test>" banners for
    /// fixture/teardown failures (pytest groups all of these together).
    fn print_collect_errors(&self) {
        let phase_errors: Vec<_> = self
            .session
            .reports
            .iter()
            .filter(|r| {
                r.outcome == Outcome::Failed
                    && matches!(r.phase, Phase::Setup | Phase::Teardown)
                    && !self
                        .session
                        .collect_errors
                        .iter()
                        .any(|(nodeid, _)| nodeid == &r.nodeid)
            })
            .collect();
        if self.session.collect_errors.is_empty() && phase_errors.is_empty() {
            return;
        }
        println!();
        println!("{}", center_banner("ERRORS"));
        for (nodeid, err) in &self.session.collect_errors {
            println!("{}", center_with(&format!("ERROR collecting {nodeid}"), '_'));
            println!("{err}");
        }
        for report in phase_errors {
            let name = report.nodeid.rsplit("::").next().unwrap_or(&report.nodeid);
            let when = match report.phase {
                Phase::Teardown => "teardown",
                _ => "setup",
            };
            println!("{}", center_with(&format!("ERROR at {when} of {name}"), '_'));
            if report.longrepr.is_some() {
                println!("{}", Self::render_longrepr(report));
            }
        }
    }

    /// --junitxml: stream every report through the LogXML writer and emit
    /// the "generated xml file" separator (hidden under -q, like pytest).
    fn write_junit_xml(&mut self, py: Python<'_>) {
        if self.config.get_value("junit-xml").is_none() || self.config.is_worker() {
            return;
        }
        match python::junit_write(py, &self.session) {
            Ok(path) => {
                if !self.config.no_terminal() && !self.config.quiet {
                    println!("{}", center_with(&format!("generated xml file: {path}"), '-'));
                }
            }
            Err(err) => {
                eprintln!("INTERNAL ERROR: {}", python::format_exception(py, &err));
            }
        }
    }

    /// A failing report's terminal text: the longrepr followed by its
    /// "Captured stdout/stderr/log {when}" sections (kept separate on the
    /// report so junitxml can read them structured).
    fn render_longrepr(report: &crate::report::TestReport) -> String {
        let mut text = report.longrepr.clone().unwrap_or_default();
        for (title, body) in &report.sections {
            text.push_str(&format!(
                "\n{:-^80}\n{}",
                format!(" {title} "),
                body.trim_end_matches('\n')
            ));
        }
        text
    }

    /// Return the display title for a failure section heading.
    /// Doctest items get a `[doctest] ` prefix matching pytest's reportinfo().
    fn failure_title(nodeid: &str) -> String {
        if !nodeid.contains("::") {
            // Text-file doctest (e.g. "test_fail.txt")
            return format!("[doctest] {nodeid}");
        }
        let after = nodeid.splitn(2, "::").nth(1).unwrap_or(nodeid);
        if after.contains('.') {
            // Module doctest (e.g. "file.py::module.Class.method")
            format!("[doctest] {after}")
        } else {
            // Regular test: use the last "::" component
            nodeid.rsplit("::").next().unwrap_or(nodeid).to_string()
        }
    }

    fn print_failures(&self) {
        let failures: Vec<_> = self
            .session
            .reports
            .iter()
            .filter(|r| r.outcome == Outcome::Failed && r.phase == Phase::Call)
            .collect();
        if failures.is_empty() {
            return;
        }
        println!("\n{}", center_banner("FAILURES"));
        for report in &failures {
            let mut name = Self::failure_title(&report.nodeid);
            // Subtest headings append the description: "test_foo [msg]".
            if let Some(desc) = &report.subtest_desc {
                name = format!("{name} {desc}");
            }
            println!("{}", center_named(&name));
            if report.longrepr.is_some() {
                println!("{}", Self::render_longrepr(report));
            }
        }
    }

    /// The -r chars with aliases expanded, in the order they were given
    /// (a -> sxXEf, A -> PpsxXEf, F/S are old aliases). Default fE.
    fn report_chars(&self) -> String {
        let given = self.config.get_value("report-chars").unwrap_or("fE");
        let mut chars = String::new();
        for c in given.chars() {
            match c {
                'a' => chars = "sxXEf".to_string(),
                'A' => chars = "PpsxXEf".to_string(),
                'N' => chars.clear(),
                'F' | 'S' => {
                    let lower = c.to_ascii_lowercase();
                    if !chars.contains(lower) {
                        chars.push(lower);
                    }
                }
                c if !chars.contains(c) => chars.push(c),
                _ => {}
            }
        }
        chars
    }

    /// -rP/-rA: the PASSES section, passed tests' captured output.
    fn print_passes(&self) {
        if !self.report_chars().contains('P') {
            return;
        }
        let passed: Vec<_> = self
            .session
            .reports
            .iter()
            .filter(|r| {
                r.outcome == Outcome::Passed
                    && r.phase == Phase::Call
                    && r.subtest_desc.is_none()
            })
            .collect();
        if passed.is_empty() {
            return;
        }
        println!("{}", center_banner("PASSES"));
        for report in passed {
            println!("{}", center_named(&Self::failure_title(&report.nodeid)));
            for (title, text) in &report.sections {
                println!("{:-^80}", format!(" {title} "));
                println!("{}", text.trim_end_matches('\n'));
            }
        }
    }

    /// The "short test summary info" section, controlled by -r chars.
    /// Groups print in the order the chars were given, matching pytest.
    fn print_short_summary(&self) {
        let chars = self.report_chars();

        let mut lines = Vec::new();
        for c in chars.chars() {
            if c == 's' {
                // Skips fold by (location, reason): "SKIPPED [2] file.py:3: x".
                // The section label comes from the first skipped report, so a
                // leading subtest skip relabels the whole group SUBSKIPPED[..]
                // (upstream show_skipped_folded uses skipped[0] for the word).
                let mut label: Option<String> = None;
                let mut skip_groups: Vec<((String, String), usize)> = Vec::new();
                for report in &self.session.reports {
                    if report.outcome != Outcome::Skipped {
                        continue;
                    }
                    if label.is_none() {
                        label = Some(match &report.subtest_desc {
                            Some(desc) => format!("SUBSKIPPED{desc}"),
                            None => "SKIPPED".to_string(),
                        });
                    }
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
                }
                let label = label.unwrap_or_else(|| "SKIPPED".to_string());
                for ((location, reason), count) in skip_groups {
                    lines.push(format!("{label} [{count}] {location}: {reason}"));
                }
                continue;
            }
            for report in &self.session.reports {
                let word = match (c, report.phase, report.outcome) {
                    ('f', Phase::Call, Outcome::Failed) => "FAILED",
                    ('E', Phase::Setup | Phase::Teardown, Outcome::Failed) => "ERROR",
                    ('x', _, Outcome::XFailed) => "XFAIL",
                    ('X', _, Outcome::XPassed) => "XPASS",
                    // 'P' selects the PASSES section, not a summary line.
                    ('p', Phase::Call, Outcome::Passed) => "PASSED",
                    _ => continue,
                };
                let word = match (&report.subtest_desc, report.outcome) {
                    (Some(desc), Outcome::Failed) => format!("SUBFAILED{desc}"),
                    (Some(desc), Outcome::Passed) => format!("SUBPASSED{desc}"),
                    (Some(desc), Outcome::XFailed) => format!("SUBXFAIL{desc}"),
                    _ => word.to_string(),
                };
                let mut line = format!("{word} {}", report.nodeid);
                // Collection errors print bare, like pytest's "ERROR file.py".
                let is_collect_error = self
                    .session
                    .collect_errors
                    .iter()
                    .any(|(nodeid, _)| nodeid == &report.nodeid);
                if !is_collect_error
                    && let Some(message) = report.longrepr.as_deref().and_then(short_message)
                {
                    line.push_str(&format!(" - {message}"));
                }
                lines.push(line);
            }
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

    /// --strict-markers / --strict via CLI or ini.
    fn strict_markers(&self) -> bool {
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
    fn strict_parametrization_ids(&self) -> bool {
        let enabled = |value: &str| matches!(value.trim(), "true" | "True" | "1");
        match self.config.get_ini("strict_parametrization_ids") {
            Some(value) => enabled(value),
            None => self.config.get_ini("strict").is_some_and(enabled),
        }
    }

    /// --strict-markers / --strict (CLI or ini): every mark must be
    /// registered in the `markers` ini option or be a builtin/bundled one.
    fn check_strict_markers(&self) -> Result<(), String> {
        if !self.strict_markers() {
            return Ok(());
        }

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

    /// --deselect / -m / -k deselection (pytest applies these before plugin
    /// collection_modifyitems hooks; --deselect runs first as its hookimpl
    /// is not trylast like the -m/-k one).
    fn apply_selection(&mut self, py: Python<'_>) -> PyResult<()> {
        if let Some(prefixes) = self.config.get_values("deselect") {
            let prefixes: Vec<String> = prefixes.iter().map(|s| s.to_string()).collect();
            // pytest matches by plain nodeid prefix (main.py).
            let (kept, removed): (Vec<_>, Vec<_>) = self
                .session
                .items
                .drain(..)
                .partition(|item| !prefixes.iter().any(|p| item.nodeid.starts_with(p.as_str())));
            self.session.items = kept;
            self.session.deselected_items.extend(removed);
        }
        if let Some(expr) = self.config.get_value("markexpr").map(str::to_string) {
            let expr = expr.trim().to_string();
            let mut error = None;
            let (kept, removed): (Vec<_>, Vec<_>) =
                self.session.items.drain(..).partition(|item| {
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
            self.session.items = kept;
            self.session.deselected_items.extend(removed);
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
            let (kept, removed): (Vec<_>, Vec<_>) =
                self.session.items.drain(..).partition(|item| {
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
            self.session.items = kept;
            self.session.deselected_items.extend(removed);
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

    /// pytest_deselected conftest/plugin hooks: called once with every item
    /// dropped by -k/-m/--lf selection (a copy, like pytest's list).
    fn fire_py_deselected(&mut self, py: Python<'_>) -> PyResult<()> {
        if self.session.deselected_items.is_empty() {
            return Ok(());
        }
        let hook_funcs: Vec<Py<pyo3::PyAny>> = self
            .session
            .py_hooks
            .iter()
            .filter(|hook| hook.name == "pytest_deselected")
            .map(|hook| hook.func.clone_ref(py))
            .collect();
        if hook_funcs.is_empty() {
            return Ok(());
        }
        let nodes: Vec<Py<pyo3::PyAny>> = self
            .session
            .deselected_items
            .iter()
            .map(|item| python::make_node(py, item))
            .collect::<PyResult<_>>()?;
        let node_list = pyo3::types::PyList::new(py, nodes.iter().map(|n| n.bind(py)))?;
        for func in &hook_funcs {
            python::call_py_hook(
                py,
                func,
                &[("items", node_list.clone().unbind().into_any())],
            )?;
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
        self.fire_py_sessionfinish(py, code)
    }

    /// pytest_sessionfinish conftest/plugin hooks (session is not modeled;
    /// hooks asking for it receive None).
    fn fire_py_sessionfinish(&mut self, py: Python<'_>, code: i32) -> PyResult<()> {
        let hook_funcs: Vec<Py<pyo3::PyAny>> = self
            .session
            .py_hooks
            .iter()
            .filter(|hook| hook.name == "pytest_sessionfinish")
            .map(|hook| hook.func.clone_ref(py))
            .collect();
        if hook_funcs.is_empty() {
            return Ok(());
        }
        let config_proxy = python::make_py_config(py, &self.config)?;
        let exitstatus = code.into_pyobject(py)?.unbind().into_any();
        for func in &hook_funcs {
            python::call_py_hook(
                py,
                func,
                &[
                    ("config", config_proxy.clone_ref(py)),
                    ("session", py.None()),
                    ("exitstatus", exitstatus.clone_ref(py)),
                ],
            )?;
        }
        Ok(())
    }

    // ---- collection ----------------------------------------------------

    /// Returns per-file collection errors (formatted).
    fn collect(&mut self, py: Python<'_>) -> Result<Vec<(PathBuf, String)>, String> {
        let rootdir = self.config.rootdir.clone();
        // No CLI paths: the `testpaths` ini (globbed against rootdir) decides
        // where collection starts; an empty glob warns and falls back to a
        // recursive search from the invocation dir, like pytest.
        let mut paths = self.config.paths.clone();
        if paths.is_empty()
            && let Some(testpaths) = self.config.get_ini("testpaths")
        {
            let entries: Vec<String> = testpaths.split_whitespace().map(str::to_string).collect();
            if !entries.is_empty() {
                let globbed = python::glob_testpaths(py, &rootdir, &entries)
                    .map_err(|err| python::format_exception(py, &err))?;
                if globbed.is_empty() {
                    let _ = python::warn_explicit_at(
                        py,
                        "PytestConfigWarning",
                        "No files were found in testpaths; consider removing or adjusting \
                         your testpaths configuration. Searching recursively from the \
                         current directory instead.",
                        &rootdir.to_string_lossy(),
                        0,
                    );
                } else {
                    paths = globbed;
                }
            }
        }
        // Relative CLI paths (and bare collection) resolve against the
        // invocation dir; rootdir only anchors node ids.
        let python_files = self.config.python_files_patterns();
        let files = crate::collect::collect_test_files(
            &self.config.invocation_dir,
            &paths,
            self.config.get_flag("collect-in-virtualenv"),
            &python_files,
            self.config.get_flag("keep-duplicates"),
        )?;

        // -p NAME (non-"no:") plugins import before conftests, like
        // pytest's cmdline plugin loading.
        let named_plugins: Vec<String> = self
            .config
            .plugin_opts
            .iter()
            .filter(|spec| !spec.starts_with("no:"))
            .cloned()
            .collect();
        if !named_plugins.is_empty()
            && let Err(err) = python::load_named_plugins(
                py,
                &named_plugins,
                Some(&self.config.invocation_dir),
                &mut self.session.registry,
                &mut self.session.py_hooks,
            )
        {
            return Err(python::format_exception(py, &err));
        }

        // Installed third-party plugins (pytest11 entry points) autoload
        // next, before conftests — pytest's setuptools plugin loading.
        let blocked: Vec<String> = self
            .config
            .plugin_opts
            .iter()
            .filter_map(|spec| spec.strip_prefix("no:"))
            .map(str::to_string)
            .collect();
        if let Err(err) = python::load_entrypoint_plugins(
            py,
            &blocked,
            &mut self.session.registry,
            &mut self.session.py_hooks,
        ) {
            return Err(python::format_exception(py, &err));
        }

        // Conftests load for every collection start dir (even ones with no
        // test files — pytest imports initial conftests during dir scan),
        // plus each collected file's directory chain.
        let mut start_dirs: Vec<PathBuf> = Vec::new();
        if paths.is_empty() {
            start_dirs.push(self.config.invocation_dir.clone());
        } else {
            for path in &paths {
                let fs_part = path.split("::").next().unwrap_or_default();
                let resolved = self.config.invocation_dir.join(fs_part);
                if resolved.is_dir() {
                    start_dirs.push(resolved);
                } else if let Some(parent) = resolved.parent() {
                    start_dirs.push(parent.to_path_buf());
                }
            }
        }
        start_dirs.extend(
            files
                .iter()
                .filter_map(|f| f.parent().map(std::path::Path::to_path_buf)),
        );

        let mut conftests: Vec<PathBuf> = Vec::new();
        for start in &start_dirs {
            let mut dir = Some(start.as_path());
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
        // pytest's catching_logs around pytest_collection: a root handler
        // during import keeps module-level logging calls from triggering
        // logging.basicConfig (issue #6240).
        let log_level_cfg: Option<String> = self
            .config
            .get_value("log-level")
            .map(str::to_string)
            .or_else(|| self.config.get_ini("log_level").map(str::to_string));
        python::log_start_phase(py, "collection", log_level_cfg.as_deref());
        for file in &files {
            // --maxfail aborts collection once the budget is spent on
            // collection errors, ignoring further files.
            if let Some(m) = self.config.maxfail()
                && errors.len() >= m
            {
                break;
            }
            let is_py = file.extension().and_then(|e| e.to_str()) == Some("py");
            if !is_py {
                // Non-Python files: only text files with doctest content.
                // For explicitly-specified files, collect regardless of --doctest-glob.
                // For scanned files, the glob loop below handles them.
                let ext = file.extension().and_then(|e| e.to_str()).unwrap_or("");
                let is_text_doctest = matches!(ext, "txt" | "rst" | "md");
                if is_text_doctest {
                    if let Ok(py_config) = python::make_py_config(py, &self.config) {
                        if let Err(err) = python::collect_doctests_from_textfile(
                            py,
                            &rootdir,
                            file,
                            &py_config,
                            &mut self.session.items,
                        ) {
                            errors.push((file.clone(), python::format_exception(py, &err)));
                        }
                    }
                }
                continue;
            }
            // Import-time output attaches to a failing collect report as
            // "Captured stdout/stderr" sections (pytest's
            // pytest_make_collect_report capture).
            python::capture_collect_begin(py);
            let collect_result = python::collect_module(
                py,
                &rootdir,
                file,
                &mut self.session.items,
                &mut self.session.registry,
                &mut self.session.py_hooks,
            );
            let collect_sections = python::capture_collect_end(py);
            let with_sections = |mut message: String| {
                for (title, text) in &collect_sections {
                    message.push_str(&format!(
                        "\n{:-^80}\n{}",
                        format!(" {title} "),
                        text.trim_end_matches('\n')
                    ));
                }
                message
            };
            let module_ok = match collect_result {
                Ok(()) => true,
                Err(err) => {
                    // pytest.skip(..., allow_module_level=True) or
                    // unittest.SkipTest at module import skip the whole module;
                    // a bare pytest.skip there is an error.
                    match python::module_level_skip(py, &err) {
                        Some(Ok(reason)) => {
                            let nodeid = crate::collect::file_nodeid(&rootdir, file);
                            // The skip call site (file:line), like pytest.
                            let location = python::raise_location(py, &err)
                                .unwrap_or_else(|| format!("{nodeid}:1"));
                            self.session.reports.push(crate::report::TestReport {
                                nodeid: nodeid.clone(),
                                phase: crate::report::Phase::Setup,
                                outcome: crate::report::Outcome::Skipped,
                                duration: std::time::Duration::ZERO,
                                longrepr: Some(reason),
                                location: Some(location),
                                subtest_desc: None,
                                sections: Vec::new(),
                            });
                        }
                        Some(Err(message)) => errors.push((file.clone(), with_sections(message))),
                        // CollectError carries a user-facing message, no traceback.
                        None => {
                            match python::collect_error_message(py, &err) {
                                Some(message) => {
                                    errors.push((file.clone(), with_sections(message)))
                                }
                                None => errors.push((
                                    file.clone(),
                                    with_sections(python::format_exception(py, &err)),
                                )),
                            }
                            // Upstream DoctestModule: with --doctest-ignore-import-errors
                            // the doctest collector skips while the Module still errors.
                            if self.config.get_flag("doctest-modules")
                                && self.config.get_flag("doctest-ignore-import-errors")
                            {
                                let nodeid = crate::collect::file_nodeid(&rootdir, file);
                                self.session.reports.push(crate::report::TestReport {
                                    nodeid: nodeid.clone(),
                                    phase: crate::report::Phase::Setup,
                                    outcome: crate::report::Outcome::Skipped,
                                    duration: std::time::Duration::ZERO,
                                    longrepr: Some(format!(
                                        "unable to import module PosixPath('{}')",
                                        file.display()
                                    )),
                                    location: Some(format!("{nodeid}:1")),
                                    subtest_desc: None,
                                    sections: Vec::new(),
                                });
                            }
                        }
                    }
                    false
                }
            };
            // --doctest-modules: collect doctests from each successfully-imported module.
            if module_ok && self.config.get_flag("doctest-modules") {
                if let Ok(py_config) = python::make_py_config(py, &self.config) {
                    if let Err(err) = python::collect_doctests_from_module(
                        py,
                        &rootdir,
                        file,
                        &py_config,
                        &mut self.session.items,
                    ) {
                        // Non-fatal: log as collect error and continue.
                        errors.push((file.clone(), python::format_exception(py, &err)));
                    }
                }
            }
        }

        // --doctest-modules: also scan ALL .py files (not just test files) for doctests.
        if self.config.get_flag("doctest-modules") {
            let extra_py = crate::collect::collect_all_python_files(
                &self.config.invocation_dir,
                &paths,
                self.config.get_flag("collect-in-virtualenv"),
                &files,
            );
            if let Ok(py_config) = python::make_py_config(py, &self.config) {
                for extra_file in &extra_py {
                    // Import the module and collect doctests.
                    if let Err(err) = python::collect_doctests_from_module(
                        py,
                        &rootdir,
                        extra_file,
                        &py_config,
                        &mut self.session.items,
                    ) {
                        // Import errors skip the module with --doctest-ignore-import-errors.
                        if self.config.get_flag("doctest-ignore-import-errors") {
                            let nodeid = crate::collect::file_nodeid(&rootdir, extra_file);
                            self.session.reports.push(crate::report::TestReport {
                                nodeid: nodeid.clone(),
                                phase: crate::report::Phase::Setup,
                                outcome: crate::report::Outcome::Skipped,
                                duration: std::time::Duration::ZERO,
                                longrepr: Some(format!(
                                    "unable to import module PosixPath('{}')",
                                    extra_file.display()
                                )),
                                location: Some(format!("{nodeid}:1")),
                                subtest_desc: None,
                                sections: Vec::new(),
                            });
                        } else {
                            errors.push((extra_file.clone(), python::format_exception(py, &err)));
                        }
                    }
                }
            }
        }

        // Text files matching the glob (default: test*.txt) are always collected
        // even without explicit --doctest-modules or --doctest-glob flags, mirroring
        // upstream pytest's _is_doctest() behavior.
        let scan_text_files = true;
        if scan_text_files {
            if let Ok(py_config) = python::make_py_config(py, &self.config) {
                let text_files =
                    crate::collect::collect_doctest_textfiles(&self.config.invocation_dir, &paths);
                for tf in text_files {
                    // Skip files already collected in the explicit-file loop above.
                    if files.contains(&tf) {
                        continue;
                    }
                    match python::is_doctest_textfile(py, &tf, &py_config) {
                        Ok(true) => {
                            if let Err(err) = python::collect_doctests_from_textfile(
                                py,
                                &rootdir,
                                &tf,
                                &py_config,
                                &mut self.session.items,
                            ) {
                                errors.push((tf.clone(), python::format_exception(py, &err)));
                            }
                        }
                        _ => {}
                    }
                }
            }
        }

        // Collection over: close its catching_logs phase.
        python::log_end_phase(py);

        // Expand items over parametrized fixtures in their closure.
        let items = std::mem::take(&mut self.session.items);
        match python::expand_fixture_params(py, items, &self.session.registry) {
            Ok(expanded) => self.session.items = expanded,
            Err(err) => return Err(python::format_exception(py, &err)),
        }

        // Node-id args ("file.py::TestCls::test_a") restrict collection to
        // matching items; unlike -k/-m this is not a deselection.
        enum ArgSel {
            Path(PathBuf),
            NodeId(String),
        }
        if paths.iter().any(|arg| arg.contains("::")) {
            let arg_sels: Vec<ArgSel> = paths
                .iter()
                .map(|arg| match arg.split_once("::") {
                    Some((file_part, rest)) => {
                        let path = self.config.invocation_dir.join(file_part);
                        let path = std::fs::canonicalize(&path).unwrap_or(path);
                        ArgSel::NodeId(format!(
                            "{}::{}",
                            crate::collect::file_nodeid(&rootdir, &path),
                            rest
                        ))
                    }
                    None => {
                        let path = self.config.invocation_dir.join(arg);
                        ArgSel::Path(std::fs::canonicalize(&path).unwrap_or(path))
                    }
                })
                .collect();
            self.session.items.retain(|item| {
                arg_sels.iter().any(|sel| match sel {
                    ArgSel::Path(path) => item.path.starts_with(path),
                    ArgSel::NodeId(sel) => {
                        item.nodeid == *sel
                            || item
                                .nodeid
                                .strip_prefix(sel.as_str())
                                .is_some_and(|rest| rest.starts_with('[') || rest.starts_with("::"))
                    }
                })
            });
        }

        // --lf drops failure-free files (and non-failed top-level functions
        // of failed files) at collection time.
        if let Some(cache) = &mut self.cache {
            cache.filter_collected_items(
                &rootdir,
                &self.config.invocation_dir,
                &paths,
                &mut self.session.items,
            );
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
    let left = (pad / 2).max(1);
    let right = (pad - pad / 2).max(1);
    format!(
        "{}{}{}",
        fill.to_string().repeat(left),
        label,
        fill.to_string().repeat(right)
    )
}
