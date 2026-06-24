use pyo3::prelude::*;

use super::super::Engine;
use super::super::{center_banner, center_named, center_with};
use crate::python;
use crate::report::{Outcome, Phase};

impl Engine {
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
        println!("{}", center_banner("FAILURES"));
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
    pub(crate) fn print_teardown_sections(&self, nodeid: &str) {
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
        println!("{}", center_banner("XFAILURES"));
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
}
