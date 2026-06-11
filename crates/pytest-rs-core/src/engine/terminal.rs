//! Terminal rendering: session header, failures, summaries, --markers.

#[allow(unused_imports)]
use super::*;
use crate::hooks::HookContext;
use crate::python;
use crate::report::{Outcome, Phase};

impl Engine {
    /// The --collect-only hierarchy: <Dir>/<Package>/<Module>/<Class>/
    /// <Function> nodes, two-space indent per level.
    /// `inspect.getdoc(obj)` split into lines (cleaned/dedented), or empty.
    fn obj_doc_lines(py: Python<'_>, obj: &Py<PyAny>) -> Vec<String> {
        py.import("inspect")
            .and_then(|inspect| inspect.call_method1("getdoc", (obj.bind(py),)))
            .ok()
            .and_then(|doc| doc.extract::<String>().ok())
            .map(|doc| doc.lines().map(str::to_string).collect())
            .unwrap_or_default()
    }

    pub(crate) fn print_collect_tree(&self, py: Python<'_>, show_docstrings: bool) {
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
                let module_class = if item.collector_class.is_empty() {
                    "Module"
                } else {
                    &item.collector_class
                };
                labels.push(format!("<{module_class} {module}>"));
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
            let last = labels.len() - 1;
            for (depth, label) in labels.iter().enumerate().skip(shared) {
                let indent = "  ".repeat(depth + 1);
                println!("{indent}{label}");
                // pytest prints each node's docstring (verbosity >= 1) on
                // the following lines, indented one level deeper. We have
                // the obj for the function (last label) and its class.
                if show_docstrings {
                    let obj = if depth == last {
                        Some(&item.func)
                    } else if label.starts_with("<Class ") {
                        item.cls.as_ref()
                    } else {
                        None
                    };
                    if let Some(obj) = obj {
                        for doc_line in Self::obj_doc_lines(py, obj) {
                            println!("{indent}  {doc_line}");
                        }
                    }
                }
            }
            open = labels;
        }
    }

    /// The --collect-only closing banner ("N/M tests collected ...", with a
    /// trailing ", K error(s)" when collection hit errors).
    pub(crate) fn print_collect_only_summary(&self, elapsed: std::time::Duration, errors: usize) {
        let selected = self.session.items.len();
        let deselected = self.session.deselected;
        let total = selected + deselected;
        let secs = elapsed.as_secs_f64();
        // pytest's collected_status: all-deselected (or nothing collected)
        // reads "no tests collected" in yellow; a partial selection shows
        // "M/T tests collected (N deselected)" in green.
        let (status, all_deselected) = if total == 0 {
            ("no tests collected".to_string(), true)
        } else if deselected == 0 {
            (
                format!(
                    "{total} test{} collected",
                    if total == 1 { "" } else { "s" }
                ),
                false,
            )
        } else if selected == 0 {
            (
                format!("no tests collected ({deselected} deselected)"),
                true,
            )
        } else {
            (
                format!("{selected}/{total} tests collected ({deselected} deselected)"),
                false,
            )
        };
        let error_suffix = if errors > 0 {
            format!(", {errors} error{}", if errors == 1 { "" } else { "s" })
        } else {
            String::new()
        };
        let body = format!("{status}{error_suffix} in {secs:.2}s");
        // Errors dominate the banner color (pytest's _color_for_type["error"]).
        let color = if errors > 0 {
            crate::tw::RED
        } else if all_deselected {
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

    /// The warnings summary. `start` skips warnings already shown (for the
    /// "(final)" pass after the short summary, which reports warnings emitted
    /// during the pytest_terminal_summary hooks). Returns the number of
    /// in-process warnings now shown, so the caller can pass it as the next
    /// `start`.
    pub(crate) fn print_warnings_summary(
        &self,
        py: Python<'_>,
        start: usize,
        final_: bool,
    ) -> usize {
        let in_process = python::warning_count(py);
        let total = in_process + self.session.worker_warning_count;
        if self.config.quiet || self.config.plugin_disabled("warnings") {
            return in_process;
        }
        let lines = python::warning_summary_lines(py, start);
        // The "(final)" pass only prints when new warnings appeared; the first
        // pass also accounts for xdist worker warnings.
        if final_ {
            if lines.is_empty() {
                return in_process;
            }
        } else if total == 0 {
            return in_process;
        }
        let title = if final_ {
            "warnings summary (final)"
        } else {
            "warnings summary"
        };
        println!("{}", center_banner(title));
        for line in lines {
            println!("{line}");
        }
        if !final_ {
            for line in &self.session.worker_warnings {
                println!("{line}");
            }
        }
        println!("-- Docs: https://docs.pytest.org/en/stable/how-to/capture-warnings.html");
        in_process
    }

    pub(crate) fn print_header(&self, py: Python<'_>) {
        if self.config.quiet || self.config.no_terminal() {
            return;
        }
        println!(
            "{}",
            crate::tw::markup(&center_banner("test session starts"), &[crate::tw::BOLD])
        );
        // --no-header keeps only the "test session starts" banner; the
        // platform/rootdir/testpaths/plugins block below is suppressed.
        if self.config.get_flag("no-header") {
            return;
        }
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
            // pytest appends a warning when other config files in the rootdir
            // also held pytest config but lost to this one.
            let warning = if self.config.ignored_config_files.is_empty() {
                String::new()
            } else {
                format!(
                    " (WARNING: ignoring pytest config in {}!)",
                    self.config.ignored_config_files.join(", ")
                )
            };
            println!("configfile: {name}{warning}");
        }
        // testpaths: shown only when collection ran from the testpaths ini
        // (no paths on the command line), like pytest's args_source check.
        if self.config.paths.is_empty()
            && let Some(testpaths) = self.config.get_ini("testpaths")
            && !testpaths.trim().is_empty()
        {
            let joined = testpaths.split_whitespace().collect::<Vec<_>>().join(", ");
            println!("testpaths: {joined}");
        }
        // plugins: dist-backed third-party plugins that autoloaded (pytest's
        // _plugin_nameversions). Omitted when none loaded — the natively
        // replaced plugins (pytest-cov, etc.) never appear.
        if !self.session.plugin_distinfo.is_empty() {
            println!("plugins: {}", self.session.plugin_distinfo.join(", "));
        }
    }

    /// The ERRORS section: "ERROR collecting <file>" banners per collection
    /// error, plus "ERROR at setup/teardown of <test>" banners for
    /// fixture/teardown failures (pytest groups all of these together).
    pub(crate) fn print_collect_errors(&self) {
        // --tb=no suppresses the traceback sections entirely (pytest's
        // summary_errors guards on tbstyle != "no").
        if self.config.get_value("tb") == Some("no") {
            return;
        }
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
                println!(
                    "{}",
                    Self::render_longrepr(
                        report,
                        self.config.get_value("show-capture").unwrap_or("all")
                    )
                );
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
    pub(crate) fn render_longrepr(
        report: &crate::report::TestReport,
        show_capture: &str,
    ) -> String {
        let mut text = report.longrepr.clone().unwrap_or_default();
        for (title, body) in &report.sections {
            // --show-capture filters which "Captured <stream> <when>" sections
            // appear on a failure (no/stdout/stderr/log/all); other sections
            // (e.g. -rA PASSES output) are always shown.
            if let Some(stream) = title.strip_prefix("Captured ") {
                let shown = match show_capture {
                    "no" => false,
                    "all" => true,
                    want => stream.starts_with(want),
                };
                if !shown {
                    continue;
                }
            }
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
        // --tb=no suppresses the FAILURES section (pytest's summary_failures
        // guards on tbstyle != "no").
        if self.config.get_value("tb") == Some("no") {
            return;
        }
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
            // A custom item's reportinfo()[2] (pytest-mypy's test_name_formatter)
            // overrides the nodeid-derived heading.
            let mut name = report
                .head_line
                .clone()
                .unwrap_or_else(|| Self::failure_title(&report.nodeid));
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
                println!(
                    "{}",
                    Self::render_longrepr(
                        report,
                        self.config.get_value("show-capture").unwrap_or("all")
                    )
                );
            }
            self.print_teardown_sections(&report.nodeid);
        }
    }

    /// pytest's _handle_teardown_sections: the "Captured ... teardown" capture
    /// sections live on the item's separate teardown report, so the terminal
    /// appends them after the call report's failure/passes repr (honoring
    /// --show-capture). Setup/call sections already rendered on the call report
    /// are skipped here by the "teardown" filter.
    fn print_teardown_sections(&self, nodeid: &str) {
        let show_capture = self.config.get_value("show-capture").unwrap_or("all");
        if show_capture == "no" {
            return;
        }
        for report in &self.session.reports {
            if report.phase != Phase::Teardown || report.nodeid != nodeid {
                continue;
            }
            for (title, body) in &report.sections {
                if !title.contains("teardown") {
                    continue;
                }
                if show_capture != "all" && !title.contains(show_capture) {
                    continue;
                }
                println!("{:-^80}", format!(" {title} "));
                println!("{}", body.trim_end_matches('\n'));
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
            self.print_teardown_sections(&report.nodeid);
        }
    }

    /// --xfail-tb: the XFAILURES section, showing the retained traceback of
    /// each xfailed test (pytest's summary_xfailures; off unless --xfail-tb,
    /// suppressed by --tb=no).
    pub(crate) fn print_xfailures(&self) {
        if !self.config.get_flag("xfail-tb") {
            return;
        }
        let style = self.config.get_value("tb").unwrap_or("auto");
        if style == "no" {
            return;
        }
        let xfailures: Vec<_> = self
            .session
            .reports
            .iter()
            .filter(|r| {
                r.outcome == Outcome::XFailed
                    && r.phase == Phase::Call
                    && r.xfail_longrepr.is_some()
            })
            .collect();
        if xfailures.is_empty() {
            return;
        }
        println!("\n{}", center_banner("XFAILURES"));
        let show_capture = self.config.get_value("show-capture").unwrap_or("all");
        for report in &xfailures {
            let tb = report.xfail_longrepr.as_deref().unwrap_or_default();
            // --tb=line: the crashline only, no per-test heading (parity with
            // summary_failures_combined's line branch).
            if style == "line" {
                println!("{tb}");
                continue;
            }
            let name = Self::failure_title(&report.nodeid);
            println!(
                "{}",
                crate::tw::markup(&center_named(&name), &[crate::tw::RED, crate::tw::BOLD])
            );
            println!("{tb}");
            for (title, body) in &report.sections {
                if let Some(stream) = title.strip_prefix("Captured ") {
                    let shown = match show_capture {
                        "no" => false,
                        "all" => true,
                        want => stream.starts_with(want),
                    };
                    if !shown {
                        continue;
                    }
                }
                println!("{:-^80}", format!(" {title} "));
                println!("{}", body.trim_end_matches('\n'));
            }
            self.print_teardown_sections(&report.nodeid);
        }
    }

    /// -rX: the XPASSES section, xpassed tests' captured output (pytest's
    /// summary_xpasses; suppressed by --tb=no, only tests with captured
    /// sections get a heading).
    pub(crate) fn print_xpasses(&self) {
        if !self.report_chars().contains('X') || self.config.get_value("tb") == Some("no") {
            return;
        }
        let xpassed: Vec<_> = self
            .session
            .reports
            .iter()
            .filter(|r| r.outcome == Outcome::XPassed && r.phase == Phase::Call)
            .collect();
        if xpassed.is_empty() {
            return;
        }
        println!("{}", center_banner("XPASSES"));
        for report in xpassed {
            if report.sections.is_empty() {
                continue;
            }
            println!(
                "{}",
                crate::tw::markup(
                    &center_named(&Self::failure_title(&report.nodeid)),
                    &[crate::tw::GREEN, crate::tw::BOLD]
                )
            );
            for (title, text) in &report.sections {
                println!("{:-^80}", format!(" {title} "));
                println!("{}", text.trim_end_matches('\n'));
            }
            self.print_teardown_sections(&report.nodeid);
        }
    }

    /// The "short test summary info" section, controlled by -r chars.
    /// Groups print in the order the chars were given, matching pytest.
    pub(crate) fn print_short_summary(&self) {
        let chars = self.report_chars();

        let mut lines = Vec::new();
        for c in chars.chars() {
            if c == 's' {
                // --no-fold-skipped lists each skip on its own line, like the
                // FAILED lines: "SKIPPED nodeid - Skipped: reason" (upstream
                // show_skipped_unfolded uses the full longrepr reason, which we
                // store stripped of its "Skipped: " prefix — re-add it).
                if self.config.get_flag("no-fold-skipped") {
                    for report in &self.session.reports {
                        if report.outcome != Outcome::Skipped {
                            continue;
                        }
                        let word = match &report.subtest_desc {
                            Some(desc) => format!("SUBSKIPPED{desc}"),
                            None => "SKIPPED".to_string(),
                        };
                        let mut line = format!("{word} {}", report.nodeid);
                        let reason = report.longrepr.clone().unwrap_or_default();
                        if !reason.is_empty() {
                            let reason = if reason.starts_with("Skipped") {
                                reason
                            } else {
                                format!("Skipped: {reason}")
                            };
                            line.push_str(&format!(" - {reason}"));
                        }
                        lines.push(line);
                    }
                    continue;
                }
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
                // Prefer reprcrash.message (always set, tb-style-independent)
                // over parsing longrepr lines (unavailable with --tb=no).
                let crash_msg = report
                    .reprcrash_message
                    .as_deref()
                    .map(str::to_string)
                    .or_else(|| report.longrepr.as_deref().and_then(short_message));
                if !is_collect_error && let Some(message) = crash_msg {
                    // pytest's _get_line_with_reprcrash_message: failure/error
                    // lines show the full crash message on CI or at -vv, but
                    // otherwise (and always under --force-short-summary) trim it
                    // to the terminal width. xfail/xpass reasons are untrimmed.
                    let trimmable = matches!(c, 'f' | 'E' | 'p');
                    let full = (running_on_ci() || self.config.verbose >= 2)
                        && !self.config.get_flag("force-short-summary");
                    if !trimmable || full {
                        line.push_str(&format!(" - {message}"));
                    } else {
                        let available =
                            crate::runner::term_width().saturating_sub(line.chars().count());
                        if let Some(trimmed) = format_trimmed(" - ", &message, available) {
                            line.push_str(&trimmed);
                        }
                    }
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
            let mut funcs: Vec<Py<PyAny>> =
                python::instance_hook_funcs(py, "pytest_terminal_summary");
            funcs.extend(
                self.session
                    .py_hooks
                    .iter()
                    .filter(|hook| hook.name == "pytest_terminal_summary")
                    .map(|hook| hook.func.clone_ref(py)),
            );
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
                    // sys.stdout; flush it so a trailing write() with no
                    // newline (pytest-mypy's --mypy-xfail stdout dump) isn't
                    // stranded in the buffer when the engine exits.
                    if let Ok(stdout) = py.import("sys").and_then(|m| m.getattr("stdout")) {
                        let _ = stdout.call_method0("flush");
                    }
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
        // pytest passes start_path (the rootdir) to report-header hooks that
        // declare it (kwarg-filtered by the callable's signature).
        let start_path = py
            .import("pathlib")?
            .getattr("Path")?
            .call1((self.config.rootdir.to_string_lossy().as_ref(),))?
            .unbind();
        for func in &hook_funcs {
            let result = python::call_py_hook(
                py,
                func,
                &[
                    ("config", config_proxy.clone_ref(py)),
                    ("start_path", start_path.clone_ref(py)),
                ],
            )?;
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

    /// pytest_report_collectionfinish py hooks: each returns a str or list
    /// of str, printed right after the "collected N items" line (the hook
    /// receives config, start_path and the collected item nodes).
    pub(crate) fn print_py_report_collectionfinish(&self, py: Python<'_>) -> PyResult<()> {
        if self.config.quiet || self.config.no_terminal() {
            return Ok(());
        }
        let hook_funcs: Vec<Py<pyo3::PyAny>> = self
            .session
            .py_hooks
            .iter()
            .filter(|hook| hook.name == "pytest_report_collectionfinish")
            .map(|hook| hook.func.clone_ref(py))
            .collect();
        if hook_funcs.is_empty() {
            return Ok(());
        }
        let config_proxy = python::make_py_config(py, &self.config)?;
        let start_path = py
            .import("pathlib")?
            .getattr("Path")?
            .call1((self.config.rootdir.to_string_lossy().as_ref(),))?
            .unbind();
        let items = pyo3::types::PyList::empty(py);
        for item in &self.session.items {
            items.append(python::make_node(py, item)?)?;
        }
        let items: Py<pyo3::PyAny> = items.into_any().unbind();
        for func in &hook_funcs {
            let result = python::call_py_hook(
                py,
                func,
                &[
                    ("config", config_proxy.clone_ref(py)),
                    ("start_path", start_path.clone_ref(py)),
                    ("items", items.clone_ref(py)),
                ],
            )?;
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

/// pytest's running_on_ci(): the CI / BUILD_NUMBER env vars suppress
/// short-summary message trimming so CI logs keep the full crash text.
fn running_on_ci() -> bool {
    std::env::var_os("CI").is_some() || std::env::var_os("BUILD_NUMBER").is_some()
}

/// pytest's _format_trimmed for the " - {}" short-summary form: ellipsize
/// `msg` to fit `available` columns after the prefix, or None when even the
/// ellipsis would not fit (so the caller drops the message entirely).
fn format_trimmed(prefix: &str, msg: &str, available: usize) -> Option<String> {
    const ELLIPSIS: &str = "...";
    let format_width = prefix.chars().count();
    if format_width + ELLIPSIS.len() > available {
        return None;
    }
    if format_width + msg.chars().count() <= available {
        return Some(format!("{prefix}{msg}"));
    }
    let budget = available - format_width - ELLIPSIS.len();
    let trimmed: String = msg.chars().take(budget).collect();
    Some(format!("{prefix}{trimmed}{ELLIPSIS}"))
}
