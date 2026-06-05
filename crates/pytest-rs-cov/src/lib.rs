//! pytest-cov equivalent: Rust-native line coverage via sys.monitoring
//! (PEP 669). Each line costs one callback ever (the callback returns
//! DISABLE), instead of coverage.py's per-line trace overhead.

mod analysis;
mod collector;
mod report;

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use collector::LineCollector;
use pytest_rs_core::config::{OptDef, OptionParser};
use pytest_rs_core::hooks::{HookContext, Plugin};
use pytest_rs_core::pyo3 as core_pyo3;
use report::{CoverageData, FileRow};

use core_pyo3::prelude::*;

/// sys.monitoring's reserved coverage tool slot.
const TOOL_ID: u8 = 1;

/// `import pytest_cov` API surface (errors/warnings only; measurement is
/// native).
const SHIM_FILES: &[(&str, &str)] = &[
    ("__init__.py", include_str!("../py/pytest_cov/__init__.py")),
    ("plugin.py", include_str!("../py/pytest_cov/plugin.py")),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReportKind {
    Term,
    TermMissing,
    Xml,
    Lcov,
}

struct ReportSpec {
    kind: ReportKind,
    dest: Option<String>,
    skip_covered: bool,
}

pub struct CovPlugin {
    enabled: bool,
    /// Absolute source roots (dirs get a trailing separator); empty = all
    /// files under rootdir that get executed.
    sources: Vec<PathBuf>,
    reports: Vec<ReportSpec>,
    fail_under: Option<f64>,
    exclude_patterns: Vec<regex::Regex>,
    collector: Option<Py<LineCollector>>,
    data: Option<CoverageData>,
    fail_under_message: Option<String>,
    /// Worker mode: this process's hits, held for pytest_worker_dump.
    dump_payload: Option<String>,
    /// Parent mode: hits merged in from workers via pytest_worker_load.
    worker_hits: HashMap<String, BTreeSet<u32>>,
}

impl CovPlugin {
    pub fn new() -> Self {
        Self {
            enabled: false,
            sources: Vec::new(),
            reports: Vec::new(),
            fail_under: None,
            exclude_patterns: Vec::new(),
            collector: None,
            data: None,
            fail_under_message: None,
            dump_payload: None,
            worker_hits: HashMap::new(),
        }
    }

    fn parse_reports(py: Python<'_>, specs: Option<Vec<&str>>) -> PyResult<Vec<ReportSpec>> {
        let Some(specs) = specs else {
            return Ok(vec![ReportSpec {
                kind: ReportKind::Term,
                dest: None,
                skip_covered: false,
            }]);
        };
        let mut reports = Vec::new();
        for spec in specs {
            if spec.is_empty() {
                // `--cov-report=` disables report output entirely.
                return Ok(Vec::new());
            }
            let (kind, dest) = match spec.split_once(':') {
                Some((kind, dest)) => (kind, Some(dest.to_string())),
                None => (spec, None),
            };
            let kind = match kind {
                "term" => ReportKind::Term,
                "term-missing" => ReportKind::TermMissing,
                "xml" => ReportKind::Xml,
                "lcov" => ReportKind::Lcov,
                other => {
                    return Err(pytest_rs_core::python::usage_error(
                        py,
                        &format!(
                            "--cov-report={other} is not supported by pytest-rs \
                             (supported: term, term-missing, xml, lcov)"
                        ),
                    ));
                }
            };
            // term/term-missing take only the skip-covered modifier, not an
            // output path.
            let mut skip_covered = false;
            let dest = match (kind, dest) {
                (ReportKind::Term | ReportKind::TermMissing, Some(modifier)) => {
                    if modifier == "skip-covered" {
                        skip_covered = true;
                        None
                    } else {
                        return Err(pytest_rs_core::python::usage_error(
                            py,
                            &format!(
                                "argument --cov-report: output specifier not supported for: \
                                 \"{spec}\""
                            ),
                        ));
                    }
                }
                (_, dest) => dest,
            };
            reports.push(ReportSpec {
                kind,
                dest,
                skip_covered,
            });
        }
        Ok(reports)
    }

    /// Effective exclude_lines regexes: from --cov-config / .coveragerc
    /// ([report] or [coverage:report]) or pyproject.toml
    /// ([tool.coverage.report]); the default pragma regex otherwise.
    fn load_exclude_patterns(rootdir: &Path, cov_config: Option<&str>) -> Vec<regex::Regex> {
        let mut patterns: Vec<String> = Vec::new();

        let rc_path = rootdir.join(cov_config.unwrap_or(".coveragerc"));
        if let Ok(content) = std::fs::read_to_string(&rc_path) {
            patterns.extend(Self::parse_coveragerc_excludes(&content));
        }
        if patterns.is_empty()
            && let Ok(content) = std::fs::read_to_string(rootdir.join("pyproject.toml"))
            && let Ok(document) = content.parse::<toml::Value>()
            && let Some(lines) = document
                .get("tool")
                .and_then(|tool| tool.get("coverage"))
                .and_then(|coverage| coverage.get("report"))
                .and_then(|report| report.get("exclude_lines"))
                .and_then(|value| value.as_array())
        {
            patterns.extend(
                lines
                    .iter()
                    .filter_map(|line| line.as_str().map(str::to_string)),
            );
        }

        if patterns.is_empty() {
            patterns.push(analysis::DEFAULT_EXCLUDE.to_string());
        }
        patterns
            .iter()
            .filter_map(|pattern| regex::Regex::new(pattern).ok())
            .collect()
    }

    /// `exclude_lines` from an ini-style coverage config ([report] section,
    /// or the [coverage:report] prefixed form used in setup.cfg/tox.ini).
    fn parse_coveragerc_excludes(content: &str) -> Vec<String> {
        let mut patterns = Vec::new();
        let mut in_report = false;
        let mut in_exclude = false;
        for line in content.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with('[') {
                in_report = trimmed == "[report]" || trimmed == "[coverage:report]";
                in_exclude = false;
                continue;
            }
            if !in_report || trimmed.starts_with('#') || trimmed.starts_with(';') {
                continue;
            }
            if let Some((key, value)) = trimmed.split_once('=') {
                in_exclude = key.trim() == "exclude_lines";
                if in_exclude && !value.trim().is_empty() {
                    patterns.push(value.trim().to_string());
                }
                continue;
            }
            if in_exclude && line.starts_with([' ', '\t']) && !trimmed.is_empty() {
                patterns.push(trimmed.to_string());
            } else if !trimmed.is_empty() {
                in_exclude = false;
            }
        }
        patterns
    }

    /// Resolve a --cov=VALUE entry: a path relative to rootdir, or a dotted
    /// module name that maps onto a directory. Canonicalized so prefix
    /// matching agrees with co_filename (which sees through symlinks like
    /// macOS /tmp).
    fn resolve_source(rootdir: &Path, value: &str) -> PathBuf {
        let as_path = rootdir.join(value);
        if as_path.exists() {
            return as_path.canonicalize().unwrap_or(as_path);
        }
        let as_module = rootdir.join(value.replace('.', "/"));
        if as_module.exists() {
            return as_module.canonicalize().unwrap_or(as_module);
        }
        as_path
    }

    /// All .py files under a source root (for 0%-covered files that were
    /// never imported).
    fn walk_py_files(root: &Path, files: &mut BTreeSet<PathBuf>) {
        if root.is_file() {
            if root.extension().is_some_and(|ext| ext == "py") {
                files.insert(root.to_path_buf());
            }
            return;
        }
        let Ok(entries) = std::fs::read_dir(root) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with('.') || name == "__pycache__" {
                continue;
            }
            if path.is_dir() {
                Self::walk_py_files(&path, files);
            } else if path.extension().is_some_and(|ext| ext == "py") {
                files.insert(path);
            }
        }
    }

    fn build_data(&self, rootdir: &Path, hits: HashMap<String, BTreeSet<u32>>) -> CoverageData {
        // Report set: every hit file, plus (with explicit --cov=src) every
        // .py file under the sources, so never-imported files show as 0%.
        let mut files: BTreeSet<PathBuf> = hits.keys().map(PathBuf::from).collect();
        for source in &self.sources {
            Self::walk_py_files(source, &mut files);
        }

        let mut rows = Vec::new();
        for path in files {
            let hits = hits.get(&path.to_string_lossy().to_string());
            let Ok(source_text) = std::fs::read_to_string(&path) else {
                continue;
            };
            let Some(analysis) = analysis::analyze(&source_text, &self.exclude_patterns) else {
                continue; // unparseable: skip rather than misreport
            };
            // Non-excluded observed lines are executable by definition;
            // the union keeps the numerator inside the denominator when
            // the analysis disagrees with CPython's actual events.
            let covered: BTreeSet<u32> = hits
                .map(|lines| lines.difference(&analysis.excluded).copied().collect())
                .unwrap_or_default();
            let mut executable = analysis.executable;
            executable.extend(covered.iter().copied());
            let name = path
                .strip_prefix(rootdir)
                .unwrap_or(&path)
                .to_string_lossy()
                .to_string();
            rows.push(FileRow {
                name,
                executable,
                covered,
            });
        }
        rows.sort_by(|a, b| a.name.cmp(&b.name));
        CoverageData { rows }
    }
}

impl Default for CovPlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl Plugin for CovPlugin {
    fn name(&self) -> &str {
        "cov"
    }

    fn pytest_addoption(&self, parser: &mut OptionParser) {
        parser.add_option(OptDef::optional_value(
            "--cov",
            "measure coverage for SOURCE (path or package name); bare --cov measures all executed files under rootdir",
        ));
        parser.add_option(OptDef::flag("--no-cov", "disable coverage"));
        parser.add_option(OptDef::optional_value(
            "--cov-report",
            "coverage report type: term, term-missing, xml[:dest], lcov[:dest]",
        ));
        parser.add_option(OptDef::value(
            "--cov-fail-under",
            None,
            "fail if total coverage is less than MIN percent",
        ));
        parser.add_option(OptDef::flag(
            "--cov-branch",
            "accepted but inert: branch coverage is not implemented yet",
        ));
        parser.add_option(OptDef::value(
            "--cov-config",
            None,
            "coverage config file (only [report] exclude_lines is read)",
        ));
        parser.add_option(OptDef::flag(
            "--cov-append",
            "accepted but inert: append mode is not implemented yet",
        ));
    }

    fn pytest_configure(&mut self, ctx: &mut HookContext) -> PyResult<()> {
        let py = ctx.py;
        // The importable pytest_cov package exists whether or not coverage
        // is enabled for this run (mirrors having pytest-cov installed).
        let package_root = pytest_rs_core::python::shim_root().join("pytest_cov");
        std::fs::create_dir_all(&package_root)
            .map_err(|e| core_pyo3::exceptions::PyOSError::new_err(e.to_string()))?;
        for (rel, content) in SHIM_FILES {
            std::fs::write(package_root.join(rel), content)
                .map_err(|e| core_pyo3::exceptions::PyOSError::new_err(e.to_string()))?;
        }

        let Some(cov_values) = ctx.config.get_values("cov") else {
            return Ok(());
        };
        if ctx.config.get_flag("no-cov") {
            return Ok(());
        }
        self.enabled = true;
        let rootdir = ctx
            .config
            .rootdir
            .canonicalize()
            .unwrap_or_else(|_| ctx.config.rootdir.clone());
        self.sources = cov_values
            .iter()
            .filter(|value| !value.is_empty())
            .map(|value| Self::resolve_source(&rootdir, value))
            .collect();
        self.reports = Self::parse_reports(py, ctx.config.get_values("cov-report"))?;
        self.fail_under = ctx
            .config
            .get_value("cov-fail-under")
            .and_then(|value| value.parse().ok());
        self.exclude_patterns =
            Self::load_exclude_patterns(&rootdir, ctx.config.get_value("cov-config"));

        let separator = std::path::MAIN_SEPARATOR.to_string();
        let with_sep = |path: &Path| {
            let mut text = path.to_string_lossy().to_string();
            if path.is_dir() && !text.ends_with(std::path::MAIN_SEPARATOR) {
                text.push_str(&separator);
            }
            text
        };

        let monitoring = py.import("sys")?.getattr("monitoring")?;
        let disable = monitoring.getattr("DISABLE")?.unbind();
        let collector = Py::new(
            py,
            LineCollector::new(
                with_sep(&rootdir),
                self.sources.iter().map(|source| with_sep(source)).collect(),
                with_sep(&pytest_rs_core::python::shim_root()),
                disable,
            ),
        )?;

        let line_event = monitoring.getattr("events")?.getattr("LINE")?;
        monitoring.call_method1("use_tool_id", (TOOL_ID, "pytest-rs-cov"))?;
        monitoring.call_method1(
            "register_callback",
            (TOOL_ID, &line_event, collector.bind(py)),
        )?;
        monitoring.call_method1("set_events", (TOOL_ID, &line_event))?;
        self.collector = Some(collector);
        Ok(())
    }

    fn pytest_sessionfinish(&mut self, ctx: &mut HookContext, _exit_code: i32) -> PyResult<()> {
        let Some(collector) = self.collector.take() else {
            return Ok(());
        };
        let py = ctx.py;
        let monitoring = py.import("sys")?.getattr("monitoring")?;
        monitoring.call_method1("set_events", (TOOL_ID, 0))?;
        monitoring.call_method1("free_tool_id", (TOOL_ID,))?;

        let mut hits = collector.borrow(py).take_hits();
        if ctx.config.is_worker() {
            // Workers don't report: hits travel to the parent for merging.
            self.dump_payload = Some(
                serde_json::to_string(&hits)
                    .map_err(|e| core_pyo3::exceptions::PyValueError::new_err(e.to_string()))?,
            );
            return Ok(());
        }
        // The parent's own hits (import-time coverage from collection)
        // merge with everything the workers measured.
        for (file, lines) in self.worker_hits.drain() {
            hits.entry(file).or_default().extend(lines);
        }
        let rootdir = ctx
            .config
            .rootdir
            .canonicalize()
            .unwrap_or_else(|_| ctx.config.rootdir.clone());
        let data = self.build_data(&rootdir, hits);

        for spec in &self.reports {
            let (default_dest, content) = match spec.kind {
                ReportKind::Xml => (
                    "coverage.xml",
                    report::render_xml(&data, &ctx.config.rootdir.to_string_lossy()),
                ),
                ReportKind::Lcov => ("coverage.lcov", report::render_lcov(&data)),
                ReportKind::Term | ReportKind::TermMissing => continue,
            };
            let dest = spec
                .dest
                .clone()
                .unwrap_or_else(|| default_dest.to_string());
            let path = ctx.config.rootdir.join(dest);
            std::fs::write(&path, content)
                .map_err(|e| core_pyo3::exceptions::PyOSError::new_err(e.to_string()))?;
        }

        if let Some(fail_under) = self.fail_under {
            let total = data.total_percent();
            if total < fail_under {
                ctx.session.exit_code_override =
                    Some(pytest_rs_core::report::exit_code::TESTS_FAILED);
                self.fail_under_message = Some(format!(
                    "FAIL Required test coverage of {fail_under}% not reached. \
                     Total coverage: {total:.2}%"
                ));
            } else {
                self.fail_under_message = Some(format!(
                    "Required test coverage of {fail_under}% reached. \
                     Total coverage: {total:.2}%"
                ));
            }
        }
        self.data = Some(data);
        Ok(())
    }

    fn pytest_worker_dump(&mut self, _ctx: &mut HookContext) -> PyResult<Option<String>> {
        Ok(self.dump_payload.take())
    }

    fn pytest_worker_load(&mut self, _ctx: &mut HookContext, payload: &str) -> PyResult<()> {
        let hits: HashMap<String, BTreeSet<u32>> = serde_json::from_str(payload)
            .map_err(|e| core_pyo3::exceptions::PyValueError::new_err(e.to_string()))?;
        for (file, lines) in hits {
            self.worker_hits.entry(file).or_default().extend(lines);
        }
        Ok(())
    }

    fn pytest_terminal_summary(&self, ctx: &mut HookContext, out: &mut String) -> PyResult<()> {
        let Some(data) = &self.data else {
            return Ok(());
        };
        let term_spec = self.reports.iter().find_map(|spec| match spec.kind {
            ReportKind::Term => Some((false, spec.skip_covered)),
            ReportKind::TermMissing => Some((true, spec.skip_covered)),
            _ => None,
        });
        if let Some((missing, skip_covered)) = term_spec {
            let version_info = ctx.py.import("sys")?.getattr("version_info")?;
            let python_version = format!(
                "{}.{}.{}-{}-{}",
                version_info.getattr("major")?,
                version_info.getattr("minor")?,
                version_info.getattr("micro")?,
                version_info.getattr("releaselevel")?,
                version_info.getattr("serial")?,
            );
            out.push_str(&report::render_term(
                data,
                missing,
                skip_covered,
                &python_version,
            ));
        }
        if let Some(message) = &self.fail_under_message {
            out.push_str(message);
            out.push('\n');
        }
        Ok(())
    }
}
