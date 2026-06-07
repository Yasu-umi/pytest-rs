//! Terminal rendering: session header, failures, summaries, --markers.

#[allow(unused_imports)]
use super::*;
use crate::hooks::HookContext;
use crate::python;
use crate::report::{Outcome, Phase};

impl Engine {
    /// The --collect-only hierarchy: <Dir>/<Package>/<Module>/<Class>/
    /// <Function> nodes, two-space indent per level.
    pub(crate) fn print_collect_tree(&self) {
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
    pub(crate) fn print_collect_only_summary(&self, elapsed: std::time::Duration) {
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
            crate::tw::YELLOW
        } else {
            crate::tw::GREEN
        };
        println!();
        if self.config.quiet {
            // -q: the bare summary line, no banner.
            println!("{}", crate::tw::markup(&body, &[color]));
        } else {
            println!("{}", crate::tw::markup(&center_banner(&body), &[color]));
        }
    }

    pub(crate) fn print_warnings_summary(&self, py: Python<'_>) {
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

    pub(crate) fn print_header(&self, py: Python<'_>) {
        if self.config.quiet || self.config.no_terminal() {
            return;
        }
        println!(
            "{}",
            crate::tw::markup(&center_banner("test session starts"), &[crate::tw::BOLD])
        );
        // Upstream's "platform darwin -- Python 3.13.2, pytest-9.0.3, ..."
        // shape (sys.platform naming), with pytest-rs as the tool.
        let platform = match std::env::consts::OS {
            "macos" => "darwin",
            "windows" => "win32",
            other => other,
        };
        let version = py.version().split_whitespace().next().unwrap_or("");
        println!(
            "platform {platform} -- Python {version}, pytest-rs-{}",
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
        if let Some(name) = &self.config.config_file_name {
            println!("configfile: {name}");
        }
    }

    /// The ERRORS section: "ERROR collecting <file>" banners per collection
    /// error, plus "ERROR at setup/teardown of <test>" banners for
    /// fixture/teardown failures (pytest groups all of these together).
    pub(crate) fn print_collect_errors(&self) {
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
            println!(
                "{}",
                center_with(&format!("ERROR collecting {nodeid}"), '_')
            );
            println!("{err}");
        }
        for report in phase_errors {
            // Upstream head_line: the domain parts (class + method) joined
            // with "." — "MyTestCase.test", "test_foo".
            let domain: Vec<&str> = report.nodeid.split("::").skip(1).collect();
            let name = if domain.is_empty() {
                report.nodeid.clone()
            } else {
                domain.join(".")
            };
            let when = match report.phase {
                Phase::Teardown => "teardown",
                _ => "setup",
            };
            println!(
                "{}",
                center_with(&format!("ERROR at {when} of {name}"), '_')
            );
            if report.longrepr.is_some() {
                println!("{}", Self::render_longrepr(report));
            }
        }
    }

    /// --junitxml: stream every report through the LogXML writer and emit
    /// the "generated xml file" separator (hidden under -q, like pytest).
    pub(crate) fn write_junit_xml(&mut self, py: Python<'_>) {
        if self.config.get_value("junit-xml").is_none() || self.config.is_worker() {
            return;
        }
        match python::junit_write(py, &self.session) {
            Ok(path) => {
                if !self.config.no_terminal() && !self.config.quiet {
                    println!(
                        "{}",
                        center_with(&format!("generated xml file: {path}"), '-')
                    );
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
    pub(crate) fn render_longrepr(report: &crate::report::TestReport) -> String {
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
    pub(crate) fn failure_title(nodeid: &str) -> String {
        if !nodeid.contains("::") {
            // Text-file doctest (e.g. "test_fail.txt")
            return format!("[doctest] {nodeid}");
        }
        let after = nodeid.split_once("::").map(|x| x.1).unwrap_or(nodeid);
        if after.contains('.') && !after.contains("::") {
            // Module doctest (e.g. "file.py::module.Class.method")
            format!("[doctest] {after}")
        } else {
            // Upstream head_line: domain parts (class + method) joined
            // with "." — "MyTestCase.test_method", "test_foo".
            after.split("::").collect::<Vec<_>>().join(".")
        }
    }

    pub(crate) fn print_failures(&self) {
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
            println!(
                "{}",
                crate::tw::markup(&center_named(&name), &[crate::tw::RED, crate::tw::BOLD])
            );
            // xdist parity: reports from -n workers open with upstream's
            // getworkerinfoline ("[gw0] darwin -- Python 3.13.2 /usr/bin/...").
            if let (Some(worker), Some(suffix)) = (
                self.session.report_workers.get(&report.nodeid),
                self.session.worker_platinfo.as_deref(),
            ) {
                println!("[gw{worker}] {suffix}");
            }
            if report.longrepr.is_some() {
                println!("{}", Self::render_longrepr(report));
            }
        }
    }

    /// The -r chars with aliases expanded, in the order they were given
    /// (a -> sxXEf, A -> PpsxXEf, F/S are old aliases). Default fE.
    pub(crate) fn report_chars(&self) -> String {
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
    pub(crate) fn print_passes(&self) {
        if !self.report_chars().contains('P') {
            return;
        }
        let passed: Vec<_> = self
            .session
            .reports
            .iter()
            .filter(|r| {
                r.outcome == Outcome::Passed && r.phase == Phase::Call && r.subtest_desc.is_none()
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
    pub(crate) fn print_short_summary(&self) {
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

    pub(crate) fn print_plugin_summaries(
        &mut self,
        py: Python<'_>,
        exitstatus: i32,
    ) -> PyResult<()> {
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
        // conftest/plugin pytest_terminal_summary impls (native mode): the
        // terminalreporter kwarg is the default stand-in writing to stdout.
        // Delegated mode replays these from _reporter.finish instead.
        if self.session.custom_reporter.is_none() {
            let funcs: Vec<Py<PyAny>> = self
                .session
                .py_hooks
                .iter()
                .filter(|hook| hook.name == "pytest_terminal_summary")
                .map(|hook| hook.func.clone_ref(py))
                .collect();
            if !funcs.is_empty() {
                let reporter = py.import("pytest._reporter")?.getattr("_default")?;
                if !reporter.is_none() {
                    let config = python::make_py_config(py, &self.config)?;
                    let status: Py<PyAny> = exitstatus.into_pyobject(py)?.into_any().unbind();
                    for func in funcs {
                        if let Err(err) = python::call_py_hook(
                            py,
                            &func,
                            &[
                                ("terminalreporter", reporter.clone().unbind()),
                                ("exitstatus", status.clone_ref(py)),
                                ("config", config.clone_ref(py)),
                            ],
                        ) {
                            eprintln!("INTERNAL ERROR: {}", python::format_exception(py, &err));
                        }
                    }
                    // The stand-in buffers through a TerminalWriter on
                    // sys.stdout; nothing to flush explicitly.
                }
            }
        }
        Ok(())
    }

    /// --markers: ini/plugin-registered marker lines first (each as
    /// `@pytest.mark.<line>`), then pytest's builtin markers verbatim.
    pub(crate) fn print_markers(&mut self, py: Python<'_>) -> PyResult<()> {
        // Read through the config proxy: configure-time
        // addinivalue_line("markers", ...) lands there, not in self.config.
        let config_proxy = python::make_py_config(py, &self.config)?;
        let value = config_proxy.bind(py).call_method1("getini", ("markers",))?;
        // getini("markers") is a linelist (upstream); tolerate a raw string.
        let registered: Vec<String> = value.extract::<Vec<String>>().or_else(|_| {
            value.extract::<Option<String>>().map(|raw| {
                raw.unwrap_or_default()
                    .lines()
                    .map(str::to_string)
                    .collect()
            })
        })?;
        for line in &registered {
            let line = line.trim();
            if !line.is_empty() {
                println!("@pytest.mark.{line}\n");
            }
        }
        for line in [
            "@pytest.mark.filterwarnings(warning): add a warning filter to the given test. see https://docs.pytest.org/en/stable/how-to/capture-warnings.html#pytest-mark-filterwarnings",
            "@pytest.mark.skip(reason=None): skip the given test function with an optional reason. Example: skip(reason=\"no way of currently testing this\") skips the test.",
            "@pytest.mark.skipif(condition, ..., *, reason=...): skip the given test function if any of the conditions evaluate to True. Example: skipif(sys.platform == 'win32') skips the test if we are on the win32 platform. See https://docs.pytest.org/en/stable/reference/reference.html#pytest-mark-skipif",
            "@pytest.mark.xfail(condition, ..., *, reason=..., run=True, raises=None, strict=strict_xfail): mark the test function as an expected failure if any of the conditions evaluate to True. Optionally specify a reason for better reporting and run=False if you don't even want to execute the test function. If only specific exception(s) are expected, you can list them in raises, and if the test fails in other ways, it will be reported as a true failure. See https://docs.pytest.org/en/stable/reference/reference.html#pytest-mark-xfail",
            "@pytest.mark.parametrize(argnames, argvalues): call a test function multiple times passing in different arguments in turn. argvalues generally needs to be a list of values if argnames specifies only one name or a list of tuples of values if argnames specifies multiple names. Example: @parametrize('arg1', [1,2]) would lead to two calls of the decorated test function, one with arg1=1 and another with arg1=2. see https://docs.pytest.org/en/stable/how-to/parametrize.html for more info and examples.",
            "@pytest.mark.usefixtures(fixturename1, fixturename2, ...): mark tests as needing all of the specified fixtures. see https://docs.pytest.org/en/stable/explanation/fixtures.html#usefixtures",
            "@pytest.mark.tryfirst: mark a hook implementation function such that the plugin machinery will try to call it first/as early as possible. DEPRECATED, use @pytest.hookimpl(tryfirst=True) instead.",
            "@pytest.mark.trylast: mark a hook implementation function such that the plugin machinery will try to call it last/as late as possible. DEPRECATED, use @pytest.hookimpl(trylast=True) instead.",
        ] {
            println!("{line}\n");
        }
        Ok(())
    }

    /// pytest_report_header py hooks: each returns a str or list of str,
    /// printed under the session header.
    pub(crate) fn print_py_report_header(&mut self, py: Python<'_>) -> PyResult<()> {
        if self.config.quiet || self.config.no_terminal() {
            return Ok(());
        }
        let hook_funcs: Vec<Py<pyo3::PyAny>> = self
            .session
            .py_hooks
            .iter()
            .filter(|hook| hook.name == "pytest_report_header")
            .map(|hook| hook.func.clone_ref(py))
            .collect();
        if hook_funcs.is_empty() {
            return Ok(());
        }
        let config_proxy = python::make_py_config(py, &self.config)?;
        for func in &hook_funcs {
            let result = python::call_py_hook(py, func, &[("config", config_proxy.clone_ref(py))])?;
            let result = result.bind(py);
            if result.is_none() {
                continue;
            }
            let lines: Vec<String> = match result.extract::<String>() {
                Ok(line) => vec![line],
                Err(_) => result.extract().unwrap_or_default(),
            };
            for block in lines {
                for line in block.split('\n') {
                    println!("{line}");
                }
            }
        }
        Ok(())
    }
}
