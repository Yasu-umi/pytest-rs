use std::path::PathBuf;

use pyo3::prelude::*;

use super::super::Engine;
use super::super::{center_banner, center_with};
use crate::python;
use crate::report::{Outcome, Phase};

impl Engine {
    /// The --collect-only hierarchy: <Dir>/<Package>/<Module>/<Class>/
    /// <Function> nodes, two-space indent per level.
    /// `inspect.getdoc(obj)` split into lines (cleaned/dedented), or empty.
    pub(crate) fn obj_doc_lines(py: Python<'_>, obj: &Py<PyAny>) -> Vec<String> {
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
        // --pyargs: the tree should only include parents in the import path
        // (the package chain reaching the collected module), not every
        // directory up to confcutdir/rootdir (upstream #11904) — so the
        // usual enclosing `<Dir root_name>` root line is skipped and the
        // topmost printed node is instead the import root itself, labeled
        // with its bestrelpath from rootdir (falling back to an absolute
        // path when unrelated, same as pytest's own bestrelpath).
        let pyargs = self.config.get_flag("pyargs");
        let base_indent = if pyargs { 0 } else { 1 };
        if !pyargs {
            let root_name = self
                .config
                .rootdir
                .file_name()
                .map(|name| name.to_string_lossy().to_string())
                .unwrap_or_default();
            println!("<Dir {root_name}>");
        }
        // Each `--pyargs` CLI argument's import root (upstream only ever
        // parents that one node directly onto the Session — see
        // `pyargs_anchor`'s doc comment); computed once up front by
        // re-resolving the original dotted arguments, not per item.
        let pyargs_anchors: Vec<PathBuf> = if pyargs {
            self.config
                .paths
                .iter()
                .filter_map(|arg| {
                    let path_part = arg.split("::").next().unwrap_or(arg);
                    let argpath = python::resolve_pyarg(py, path_part)?;
                    Some(python::pyargs_anchor(&argpath, path_part))
                })
                .collect()
        } else {
            Vec::new()
        };
        let push_dir_labels = |labels: &mut Vec<String>, start: &std::path::Path, dirs: &[&str]| {
            let mut dir_so_far = start.to_path_buf();
            for dir in dirs {
                dir_so_far = dir_so_far.join(dir);
                let kind = if dir_so_far.join("__init__.py").is_file() {
                    "Package"
                } else {
                    "Dir"
                };
                labels.push(format!("<{kind} {dir}>"));
            }
        };
        // The open chain of (label) nodes above the current item.
        let mut open: Vec<String> = Vec::new();
        for item in &self.session.items {
            let (file_part, rest) = match item.nodeid.split_once("::") {
                Some(parts) => parts,
                None => continue,
            };
            let mut labels: Vec<String> = Vec::new();
            let segments: Vec<&str> = file_part.split('/').collect();
            let file_dir = item.path.parent().unwrap_or(&self.config.rootdir);
            let anchor = pyargs_anchors
                .iter()
                .filter(|a| file_dir.starts_with(a.as_path()))
                .max_by_key(|a| a.as_os_str().len());
            if let Some(anchor) = anchor {
                let kind = if anchor.join("__init__.py").is_file() {
                    "Package"
                } else {
                    "Dir"
                };
                let label_path = crate::collect::bestrelpath(&self.config.rootdir, anchor);
                labels.push(format!("<{kind} {label_path}>"));
                if let Ok(rel) = file_dir.strip_prefix(anchor) {
                    let dirs: Vec<&str> = rel
                        .components()
                        .map(|c| c.as_os_str().to_str().unwrap_or_default())
                        .collect();
                    push_dir_labels(&mut labels, anchor, &dirs);
                }
            } else {
                push_dir_labels(
                    &mut labels,
                    &self.config.rootdir,
                    &segments[..segments.len().saturating_sub(1)],
                );
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
                let func_class = if item.func_class.is_empty() {
                    "Function"
                } else {
                    &item.func_class
                };
                labels.push(format!("<{func_class} {function}>"));
            }
            // Print only the suffix that differs from the previous item. The
            // leaf label is never collapsed even for a fully-duplicate item
            // (--keep-duplicates can produce two consecutive identical
            // nodeids), so a repeated item still shows its own leaf line.
            let shared = open
                .iter()
                .zip(labels.iter())
                .take_while(|(open_label, label)| open_label == label)
                .count()
                .min(labels.len() - 1);
            let last = labels.len() - 1;
            for (depth, label) in labels.iter().enumerate().skip(shared) {
                let indent = "  ".repeat(depth + base_indent);
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

    pub(crate) fn print_header(&mut self, py: Python<'_>) {
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
        // Upstream's "platform darwin -- Python 3.13.2, pytest-9.0.3, pluggy-1.6.0"
        let platform = match std::env::consts::OS {
            "macos" => "darwin",
            "windows" => "win32",
            other => other,
        };
        let version = py.version().split_whitespace().next().unwrap_or("");
        println!("platform {platform} -- Python {version}, pytest-9.0.3, pluggy-1.6.0");
        // Native plugins' pytest_report_header lines (e.g. pytest-benchmark's
        // "benchmark: ...") print here, right after the platform line —
        // matching where real pytest-benchmark's own hookimpl lands once
        // pluggy's LIFO call order is reversed for display. This is the only
        // native plugin implementing this hook today, so a fixed position is
        // enough (no general hook-priority replication needed).
        if let Ok(lines) = self.native_plugin_header_lines(py) {
            for line in lines {
                println!("{line}");
            }
        }
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
        // --traceconfig: show registered plugins (minimal list; mirrors what
        // real pytest's _pytest.helpconfig shows in pytest_report_header).
        if self.config.get_flag("traceconfig") {
            println!(
                "using: pytest-9.0.3 (pytest-rs-{}, embedded)",
                env!("CARGO_PKG_VERSION")
            );
            println!("active plugins:");
            println!(
                "    pytest-rs               : pytest-rs-{}",
                env!("CARGO_PKG_VERSION")
            );
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
        // Blank line separating the section from the preceding "collected N
        // items / M errors" line (or the test-progress output).
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
}
