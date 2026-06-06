//! pytest-cov equivalent: Rust-native line coverage via sys.monitoring
//! (PEP 669). Each line costs one callback ever (the callback returns
//! DISABLE), instead of coverage.py's per-line trace overhead.

mod analysis;
mod collector;
mod report;

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use collector::{ArcMap, LineCollector};
use pytest_rs_core::config::{OptDef, OptionParser};
use pytest_rs_core::hooks::{HookContext, Plugin};
use pytest_rs_core::pyo3 as core_pyo3;
use report::{CoverageData, FileRow};

use core_pyo3::prelude::*;

/// Worker -> parent coverage payload (hits and, in branch mode, arcs).
#[derive(serde::Serialize, serde::Deserialize)]
struct CovDump {
    hits: HashMap<String, BTreeSet<u32>>,
    arcs: ArcMap,
}

/// sys.monitoring's reserved coverage tool slot.
const TOOL_ID: u8 = 1;

/// `import pytest_cov` API surface (errors/warnings only; measurement is
/// native).
const SHIM_FILES: &[(&str, &str)] = &[
    ("__init__.py", include_str!("../py/pytest_cov/__init__.py")),
    ("plugin.py", include_str!("../py/pytest_cov/plugin.py")),
    ("_child.py", include_str!("../py/pytest_cov/_child.py")),
];

/// Site .pth hook for subprocess coverage: a no-op unless the running
/// pytest-rs session exported the activation env vars (pytest-cov ships
/// the same kind of hook at install time).
const PTH_LINE: &str = "import os, runpy; os.environ.get(\"PYTEST_RS_COV_OUT\") and os.environ.get(\"PYTEST_RS_COV_CHILD\") and runpy.run_path(os.environ[\"PYTEST_RS_COV_CHILD\"], run_name=\"pytest_rs_cov_child\")\n";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReportKind {
    Term,
    TermMissing,
    Annotate,
    Html,
    Json,
    Markdown,
    MarkdownAppend,
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
    /// Parent mode: executed branch arcs merged in from workers.
    worker_arcs: ArcMap,
    /// Branch coverage (--cov-branch / [run] branch = true).
    branch: bool,
    /// Dump directory for subprocess coverage (children write, we merge).
    child_out_dir: Option<PathBuf>,
    /// coverage [paths] groups (canonical first).
    path_aliases: Vec<Vec<String>>,
    /// "Coverage X written to ..." lines for the terminal section.
    report_messages: Vec<String>,
    /// The sys.monitoring tool id actually claimed (COVERAGE_ID unless a
    /// .pth child collector from an outer session got there first).
    tool_id: u8,
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
            worker_arcs: HashMap::new(),
            branch: false,
            child_out_dir: None,
            path_aliases: Vec::new(),
            report_messages: Vec::new(),
            tool_id: TOOL_ID,
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
                "annotate" => ReportKind::Annotate,
                "html" => ReportKind::Html,
                "json" => ReportKind::Json,
                "markdown" => ReportKind::Markdown,
                "markdown-append" => ReportKind::MarkdownAppend,
                other => {
                    return Err(pytest_rs_core::python::usage_error(
                        py,
                        &format!(
                            "--cov-report={other} is not supported by pytest-rs \
                             (supported: term, term-missing, annotate, html, xml, json, \
                             lcov, markdown, markdown-append)"
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
        // pytest-cov parity: markdown and markdown-append on one file clash.
        let markdown_dest = |target: ReportKind| {
            reports
                .iter()
                .filter(move |r| r.kind == target)
                .filter_map(|r| r.dest.clone())
                .collect::<std::collections::HashSet<_>>()
        };
        if markdown_dest(ReportKind::Markdown)
            .intersection(&markdown_dest(ReportKind::MarkdownAppend))
            .next()
            .is_some()
        {
            return Err(pytest_rs_core::python::usage_error(
                py,
                "markdown and markdown-append options cannot point to the same file.",
            ));
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

    /// coverage `[run] relative_files`: from --cov-config / .coveragerc /
    /// setup.cfg / tox.ini (ini forms) or pyproject [tool.coverage.run].
    fn relative_files_enabled(rootdir: &Path, cov_config: Option<&str>) -> bool {
        Self::run_option_enabled(rootdir, cov_config, "relative_files")
    }

    /// coverage `[run] branch`, same config sources.
    fn branch_enabled(rootdir: &Path, cov_config: Option<&str>) -> bool {
        Self::run_option_enabled(rootdir, cov_config, "branch")
    }

    /// coverage `[paths]` groups: each is (canonical, aliases) — measured
    /// paths under an alias report as the canonical path (subset of
    /// coverage.py's path aliasing: literal prefixes, no globs).
    fn paths_aliases(rootdir: &Path, cov_config: Option<&str>) -> Vec<Vec<String>> {
        let mut groups: Vec<Vec<String>> = Vec::new();
        for candidate in [cov_config.unwrap_or(".coveragerc"), "setup.cfg", "tox.ini"] {
            let Ok(content) = std::fs::read_to_string(rootdir.join(candidate)) else {
                continue;
            };
            let mut in_paths = false;
            let mut current: Vec<String> = Vec::new();
            for line in content.lines() {
                let trimmed = line.trim();
                if trimmed.starts_with('[') {
                    if !current.is_empty() {
                        groups.push(std::mem::take(&mut current));
                    }
                    in_paths = trimmed == "[paths]" || trimmed == "[coverage:paths]";
                    continue;
                }
                if !in_paths || trimmed.is_empty() || trimmed.starts_with(['#', ';']) {
                    continue;
                }
                if let Some((_, value)) = trimmed.split_once('=') {
                    if !current.is_empty() {
                        groups.push(std::mem::take(&mut current));
                    }
                    if !value.trim().is_empty() {
                        current.push(value.trim().to_string());
                    }
                } else if line.starts_with([' ', '\t']) {
                    current.push(trimmed.to_string());
                }
            }
            if !current.is_empty() {
                groups.push(current);
            }
            if !groups.is_empty() {
                return groups;
            }
        }
        if let Ok(content) = std::fs::read_to_string(rootdir.join("pyproject.toml"))
            && let Ok(document) = content.parse::<toml::Table>()
            && let Some(paths) = document
                .get("tool")
                .and_then(|tool| tool.get("coverage"))
                .and_then(|coverage| coverage.get("paths"))
                .and_then(|paths| paths.as_table())
        {
            for value in paths.values() {
                if let Some(items) = value.as_array() {
                    let group: Vec<String> = items
                        .iter()
                        .filter_map(|item| item.as_str().map(str::to_string))
                        .collect();
                    if group.len() > 1 {
                        groups.push(group);
                    }
                }
            }
        }
        groups
    }

    /// A string `[run]` option, same config sources as run_option_enabled.
    fn run_option_value(rootdir: &Path, cov_config: Option<&str>, option: &str) -> Option<String> {
        for candidate in [cov_config.unwrap_or(".coveragerc"), "setup.cfg", "tox.ini"] {
            let Ok(content) = std::fs::read_to_string(rootdir.join(candidate)) else {
                continue;
            };
            let mut in_run = false;
            for line in content.lines() {
                let trimmed = line.trim();
                if trimmed.starts_with('[') {
                    in_run = trimmed == "[run]" || trimmed == "[coverage:run]";
                    continue;
                }
                if in_run
                    && let Some((key, value)) = trimmed.split_once('=')
                    && key.trim() == option
                {
                    return Some(value.trim().trim_matches(['"', '\'']).to_string());
                }
            }
        }
        if let Ok(content) = std::fs::read_to_string(rootdir.join("pyproject.toml"))
            && let Ok(document) = content.parse::<toml::Table>()
            && let Some(value) = document
                .get("tool")
                .and_then(|tool| tool.get("coverage"))
                .and_then(|coverage| coverage.get("run"))
                .and_then(|run| run.get(option))
        {
            return value
                .as_str()
                .map(str::to_string)
                .or_else(|| Some(value.to_string()));
        }
        None
    }

    /// A boolean `[run]` option: from --cov-config / .coveragerc /
    /// setup.cfg / tox.ini (ini forms) or pyproject [tool.coverage.run].
    fn run_option_enabled(rootdir: &Path, cov_config: Option<&str>, option: &str) -> bool {
        let truthy = |value: &str| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "true" | "1" | "yes" | "on"
            )
        };
        for candidate in [cov_config.unwrap_or(".coveragerc"), "setup.cfg", "tox.ini"] {
            let Ok(content) = std::fs::read_to_string(rootdir.join(candidate)) else {
                continue;
            };
            let mut in_run = false;
            for line in content.lines() {
                let trimmed = line.trim();
                if trimmed.starts_with('[') {
                    in_run = trimmed == "[run]" || trimmed == "[coverage:run]";
                    continue;
                }
                if in_run
                    && let Some((key, value)) = trimmed.split_once('=')
                    && key.trim() == option
                {
                    return truthy(value);
                }
            }
        }
        if let Ok(content) = std::fs::read_to_string(rootdir.join("pyproject.toml"))
            && let Ok(document) = content.parse::<toml::Table>()
            && let Some(value) = document
                .get("tool")
                .and_then(|tool| tool.get("coverage"))
                .and_then(|coverage| coverage.get("run"))
                .and_then(|run| run.get(option))
        {
            return value
                .as_bool()
                .unwrap_or_else(|| value.as_str().is_some_and(truthy));
        }
        false
    }

    /// Reports rendered by the coverage package from the freshly written
    /// `.coverage` data file (annotate/html/json/markdown). Returns the
    /// "Coverage ... written" line, or None when coverage isn't installed
    /// (a warning was already issued for the data file).
    fn coverage_package_report(
        &self,
        ctx: &mut HookContext,
        kind: ReportKind,
        dest: Option<&str>,
    ) -> PyResult<Option<String>> {
        let py = ctx.py;
        let Ok(coverage_mod) = py.import("coverage") else {
            return Ok(None);
        };
        let data_path = Self::data_file_path(ctx);
        if !data_path.exists() {
            return Ok(None);
        }
        let kwargs = core_pyo3::types::PyDict::new(py);
        kwargs.set_item("data_file", data_path.to_string_lossy().as_ref())?;
        let cov = coverage_mod.getattr("Coverage")?.call((), Some(&kwargs))?;
        cov.call_method0("load")?;
        let rootdir = &ctx.config.rootdir;
        let message = match kind {
            ReportKind::Html => {
                let dir = dest.unwrap_or("htmlcov");
                let kwargs = core_pyo3::types::PyDict::new(py);
                kwargs.set_item("directory", rootdir.join(dir).to_string_lossy().as_ref())?;
                cov.call_method("html_report", (), Some(&kwargs))?;
                format!("Coverage HTML written to dir {dir}")
            }
            ReportKind::Annotate => {
                let kwargs = core_pyo3::types::PyDict::new(py);
                if let Some(dir) = dest {
                    kwargs.set_item("directory", rootdir.join(dir).to_string_lossy().as_ref())?;
                }
                cov.call_method("annotate", (), Some(&kwargs))?;
                match dest {
                    Some(dir) => format!("Coverage annotated source written to dir {dir}"),
                    None => "Coverage annotated source written next to source".to_string(),
                }
            }
            ReportKind::Json => {
                let file = dest.unwrap_or("coverage.json");
                let kwargs = core_pyo3::types::PyDict::new(py);
                kwargs.set_item("outfile", rootdir.join(file).to_string_lossy().as_ref())?;
                cov.call_method("json_report", (), Some(&kwargs))?;
                format!("Coverage JSON written to file {file}")
            }
            ReportKind::Markdown | ReportKind::MarkdownAppend => {
                let append = kind == ReportKind::MarkdownAppend;
                let file = dest.unwrap_or("coverage.md");
                let handle = py.import("builtins")?.call_method1(
                    "open",
                    (
                        rootdir.join(file).to_string_lossy().as_ref(),
                        if append { "a" } else { "w" },
                    ),
                )?;
                let kwargs = core_pyo3::types::PyDict::new(py);
                kwargs.set_item("output_format", "markdown")?;
                kwargs.set_item("file", &handle)?;
                cov.call_method("report", (), Some(&kwargs))?;
                handle.call_method0("close")?;
                if append {
                    format!("Coverage Markdown information appended to file {file}")
                } else {
                    format!("Coverage Markdown information written to file {file}")
                }
            }
            _ => unreachable!("delegated kinds only"),
        };
        Ok(Some(message))
    }

    /// --cov-append: fold the previous runs' lines (already merged into
    /// the data file by write_data_file) back into this run's hits so the
    /// terminal/xml reports show the union, like pytest-cov.
    fn merge_appended_data(
        &self,
        ctx: &mut HookContext,
        hits: &mut HashMap<String, BTreeSet<u32>>,
    ) -> PyResult<()> {
        let py = ctx.py;
        let Ok(coverage_mod) = py.import("coverage") else {
            return Ok(());
        };
        let data_path = Self::data_file_path(ctx);
        if !data_path.exists() {
            return Ok(());
        }
        let data = coverage_mod
            .getattr("CoverageData")?
            .call1((data_path.to_string_lossy().as_ref(),))?;
        data.call_method0("read")?;
        let rootdir = &ctx.config.rootdir;
        for file in data.call_method0("measured_files")?.try_iter()? {
            let file: String = file?.extract()?;
            let lines: Option<Vec<u32>> = data.call_method1("lines", (&file,))?.extract()?;
            let Some(lines) = lines else { continue };
            // Data may hold rootdir-relative paths ([run] relative_files).
            let absolute = if Path::new(&file).is_absolute() {
                file
            } else {
                rootdir.join(&file).to_string_lossy().to_string()
            };
            hits.entry(absolute).or_default().extend(lines);
        }
        Ok(())
    }

    /// The `.coverage` data file location (COVERAGE_FILE honored).
    fn data_file_path(ctx: &HookContext) -> PathBuf {
        match std::env::var("COVERAGE_FILE") {
            Ok(value) => ctx.config.rootdir.join(value),
            Err(_) => ctx.config.rootdir.join(".coverage"),
        }
    }

    /// pytest-cov parity: persist the merged hits as a `.coverage` data file
    /// (coverage.py's sqlite format) via the installed `coverage` package,
    /// so downstream tooling (coverage html/report/combine, diff-cover)
    /// keeps working. Skipped with a warning when coverage isn't installed.
    fn write_data_file(
        &self,
        ctx: &mut HookContext,
        hits: &HashMap<String, BTreeSet<u32>>,
    ) -> PyResult<()> {
        let py = ctx.py;
        let Ok(coverage_mod) = py.import("coverage") else {
            let _ = pytest_rs_core::python::warn_explicit_at(
                py,
                "PytestConfigWarning",
                "coverage package not installed; .coverage data file not written",
                "pytest_cov/plugin.py",
                0,
            );
            return Ok(());
        };
        let data_path = Self::data_file_path(ctx);
        let append = ctx.config.get_flag("cov-append") && data_path.exists();
        if !append {
            let _ = std::fs::remove_file(&data_path);
        }
        let data = coverage_mod
            .getattr("CoverageData")?
            .call1((data_path.to_string_lossy().as_ref(),))?;
        if append {
            data.call_method0("read")?;
        }
        // [run] relative_files: store rootdir-relative paths so coverage
        // report/combine resolve them like coverage.py would.
        let relative =
            Self::relative_files_enabled(&ctx.config.rootdir, ctx.config.get_value("cov-config"));
        let rootdir_canon = ctx
            .config
            .rootdir
            .canonicalize()
            .unwrap_or_else(|_| ctx.config.rootdir.clone());
        let lines = core_pyo3::types::PyDict::new(py);
        for (file, hit_lines) in hits {
            let key = if relative {
                Path::new(file)
                    .strip_prefix(&rootdir_canon)
                    .map(|p| p.to_string_lossy().to_string())
                    .unwrap_or_else(|_| file.clone())
            } else {
                file.clone()
            };
            let list: Vec<u32> = hit_lines.iter().copied().collect();
            lines.set_item(key, list)?;
        }
        data.call_method1("add_lines", (lines,))?;
        data.call_method0("write")?;
        Ok(())
    }

    fn build_data(
        &self,
        rootdir: &Path,
        hits: HashMap<String, BTreeSet<u32>>,
        arcs: ArcMap,
    ) -> CoverageData {
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
            // Runtime events on continuation lines fold onto the
            // statement's first line (coverage.py's multiline map); the
            // union then keeps the numerator inside the denominator when
            // the analysis disagrees with CPython's actual events.
            let fold = |line: u32| analysis.multiline.get(&line).copied().unwrap_or(line);
            let covered: BTreeSet<u32> = hits
                .map(|lines| {
                    lines
                        .iter()
                        .map(|line| fold(*line))
                        .filter(|line| !analysis.excluded.contains(line))
                        .collect()
                })
                .unwrap_or_default();
            let mut executable = analysis.executable;
            executable.extend(covered.iter().copied());
            // Branch mode: reconcile executed bytecode arcs against the
            // source-level branch map. Arcs whose source line is not a
            // branch point (asserts, ternaries) are ignored; destinations
            // with no exact match attribute to EXIT when the branch can
            // leave the scope.
            let branches = if self.branch {
                analysis.branches
            } else {
                Default::default()
            };
            let mut taken: std::collections::BTreeMap<u32, BTreeSet<i64>> = Default::default();
            if self.branch
                && let Some(file_arcs) = arcs.get(&path.to_string_lossy().to_string())
            {
                for (src, dst, direction) in file_arcs {
                    let src = &fold(*src);
                    let dst = &if *dst > 0 {
                        i64::from(fold(*dst as u32))
                    } else {
                        *dst
                    };
                    let Some(dests) = branches.get(src) else {
                        continue;
                    };
                    let resolved = match direction {
                        // Fall-through: into the body. An arc staying on
                        // the header line is a loop's advance machinery; a
                        // same-line arc elsewhere is a short-circuit
                        // (and/or) step, not a branch outcome.
                        1 => {
                            if *dst == dests[0] {
                                Some(dests[0])
                            } else if *dst == i64::from(*src) {
                                analysis.loops.contains(src).then(|| dests[0])
                            } else {
                                Some(dests[0])
                            }
                        }
                        // Jump: away from the body. Implicit-return
                        // attribution can make the destination look like a
                        // body line; never resolve a jump to the body.
                        2 => dests[1..]
                            .iter()
                            .find(|d| *d == dst)
                            .or_else(|| dests[1..].iter().find(|d| **d == analysis::EXIT))
                            .or(dests.last())
                            .copied(),
                        // Unknown (3.13 without dis info): exact, loop
                        // advance, then exit.
                        _ => {
                            if dests.contains(dst) {
                                Some(*dst)
                            } else if *dst == i64::from(*src) && analysis.loops.contains(src) {
                                Some(dests[0])
                            } else if dests.contains(&analysis::EXIT) {
                                Some(analysis::EXIT)
                            } else {
                                None
                            }
                        }
                    };
                    if let Some(dest) = resolved {
                        taken.entry(*src).or_default().insert(dest);
                    }
                }
            }
            let mut name = path
                .strip_prefix(rootdir)
                .unwrap_or(&path)
                .to_string_lossy()
                .to_string();
            // [paths] aliasing: an alias prefix reports as the canonical one.
            'groups: for group in &self.path_aliases {
                let canonical = &group[0];
                for alias in &group[1..] {
                    let prefix = format!("{}{}", alias.trim_end_matches('/'), '/');
                    if let Some(rest) = name.strip_prefix(&prefix) {
                        name = format!("{}/{rest}", canonical.trim_end_matches('/'));
                        break 'groups;
                    }
                }
            }
            rows.push(FileRow {
                name,
                executable,
                covered,
                branches,
                taken,
            });
        }
        rows.sort_by(|a, b| a.name.cmp(&b.name));
        CoverageData {
            rows,
            branch: self.branch,
        }
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
            "measure branch coverage in addition to line coverage",
        ));
        parser.add_option(OptDef::value(
            "--cov-config",
            None,
            "coverage config file (only [report] exclude_lines is read)",
        ));
        parser.add_option(OptDef::flag(
            "--cov-append",
            "do not delete coverage but append to current (combined report)",
        ));
        parser.add_option(OptDef::flag(
            "--no-cov-on-fail",
            "accepted but inert: reports are cheap enough to always print",
        ));
        parser.add_option(OptDef::value(
            "--cov-context",
            None,
            "accepted but inert: dynamic contexts are not implemented",
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

        self.branch = ctx.config.get_flag("cov-branch")
            || Self::branch_enabled(&ctx.config.rootdir, ctx.config.get_value("cov-config"));
        self.path_aliases =
            Self::paths_aliases(&ctx.config.rootdir, ctx.config.get_value("cov-config"));

        // pytest-cov parity for dynamic_context=test_function in the
        // coverage config: fatal under xdist, a warning otherwise (the
        // --cov-context option is the supported spelling).
        if Self::run_option_value(
            &ctx.config.rootdir,
            ctx.config.get_value("cov-config"),
            "dynamic_context",
        )
        .as_deref()
            == Some("test_function")
        {
            if ctx.config.get_value("numprocesses").is_some() && !ctx.config.is_worker() {
                let message = "Detected dynamic_context=test_function in coverage configuration. \
                     This is known to cause issues when using xdist, see: \
                     https://github.com/pytest-dev/pytest-cov/issues/604\n\
                     It is recommended to use --cov-context instead.";
                eprintln!("pytest_cov.DistCovError: {message}");
                return Err(pytest_rs_core::python::usage_error(py, message));
            }
            let message = "Detected dynamic_context=test_function in coverage configuration. \
                 This is unnecessary as this plugin provides the more complete \
                 --cov-context option.";
            let category = py
                .import("pytest_cov")?
                .getattr("CentralCovContextWarning")?;
            py.import("warnings")?.call_method1(
                "warn_explicit",
                (message, category, "pytest_cov/plugin.py", 0),
            )?;
        }

        let monitoring = py.import("sys")?.getattr("monitoring")?;
        let events = monitoring.getattr("events")?;
        let disable = monitoring.getattr("DISABLE")?.unbind();
        let py_start_event = events.getattr("PY_START")?;
        let line_event = events.getattr("LINE")?;
        // Branch events: 3.14 has per-direction BRANCH_LEFT/BRANCH_RIGHT
        // (independently DISABLEable); 3.13 only the combined BRANCH.
        let mut local_events: i64 = line_event.extract()?;
        let mut branch_events: Vec<(Bound<'_, core_pyo3::PyAny>, &str)> = Vec::new();
        let mut need_jump_targets = false;
        if self.branch {
            match (
                events.getattr("BRANCH_LEFT"),
                events.getattr("BRANCH_RIGHT"),
            ) {
                (Ok(left), Ok(right)) => {
                    local_events |= left.extract::<i64>()? | right.extract::<i64>()?;
                    branch_events.push((left, "branch_left"));
                    branch_events.push((right, "branch_right"));
                }
                _ => {
                    let combined = events.getattr("BRANCH")?;
                    local_events |= combined.extract::<i64>()?;
                    branch_events.push((combined, "branch_compat"));
                    need_jump_targets = true;
                }
            }
        }
        // COVERAGE_ID, unless an outer session's .pth child collector holds
        // it (nested pytest-rs runs); free slots 3-5 are fallbacks.
        self.tool_id = [TOOL_ID, 3, 4, 5]
            .into_iter()
            .find(|candidate| {
                monitoring
                    .call_method1("use_tool_id", (*candidate, "pytest-rs-cov"))
                    .is_ok()
            })
            .ok_or_else(|| {
                core_pyo3::exceptions::PyRuntimeError::new_err(
                    "no free sys.monitoring tool id for coverage",
                )
            })?;
        let collector = Py::new(
            py,
            LineCollector::new(
                with_sep(&rootdir),
                self.sources.iter().map(|source| with_sep(source)).collect(),
                with_sep(&pytest_rs_core::python::shim_root()),
                self.branch,
                need_jump_targets,
                local_events,
                disable,
                monitoring.clone().unbind(),
                self.tool_id,
            ),
        )?;

        monitoring.call_method1(
            "register_callback",
            (
                self.tool_id,
                &py_start_event,
                collector.bind(py).getattr("py_start")?,
            ),
        )?;
        monitoring.call_method1(
            "register_callback",
            (
                self.tool_id,
                &line_event,
                collector.bind(py).getattr("line")?,
            ),
        )?;
        for (event, method) in &branch_events {
            monitoring.call_method1(
                "register_callback",
                (self.tool_id, event, collector.bind(py).getattr(*method)?),
            )?;
        }
        // Globally only the PY_START gate; LINE events arm per tracked code
        // object (coverage.py's sysmon core layout).
        monitoring.call_method1("set_events", (self.tool_id, &py_start_event))?;
        monitoring.call_method0("restart_events")?;
        self.collector = Some(collector);

        // Subprocess coverage: python children self-measure via the site
        // .pth hook (a no-op without these env vars) and dump for merging.
        let out_dir = std::env::temp_dir().join(format!("pytest-rs-cov-{}", std::process::id()));
        std::fs::create_dir_all(&out_dir)
            .map_err(|e| core_pyo3::exceptions::PyOSError::new_err(e.to_string()))?;
        let environ = py.import("os")?.getattr("environ")?;
        environ.set_item("PYTEST_RS_COV_OUT", out_dir.to_string_lossy())?;
        environ.set_item(
            "PYTEST_RS_COV_CHILD",
            pytest_rs_core::python::shim_root()
                .join("pytest_cov")
                .join("_child.py")
                .to_string_lossy(),
        )?;
        environ.set_item("PYTEST_RS_COV_ROOT", with_sep(&rootdir))?;
        environ.set_item(
            "PYTEST_RS_COV_SOURCES",
            self.sources
                .iter()
                .map(|source| with_sep(source))
                .collect::<Vec<_>>()
                .join(":"),
        )?;
        environ.set_item("PYTEST_RS_COV_BRANCH", if self.branch { "1" } else { "0" })?;
        self.child_out_dir = Some(out_dir);
        // The hook itself goes into the environment's site-packages once
        // (pytest-cov installs its equivalent at package-install time).
        if let Ok(paths) = py.import("sysconfig")?.call_method0("get_paths")
            && let Ok(Some(purelib)) = paths
                .get_item("purelib")
                .map(|p| p.extract::<String>().ok())
        {
            let _ = std::fs::write(Path::new(&purelib).join("pytest-rs-cov.pth"), PTH_LINE);
        }
        Ok(())
    }

    fn pytest_sessionfinish(&mut self, ctx: &mut HookContext, _exit_code: i32) -> PyResult<()> {
        let Some(collector) = self.collector.take() else {
            return Ok(());
        };
        let py = ctx.py;
        let monitoring = py.import("sys")?.getattr("monitoring")?;
        monitoring.call_method1("set_events", (self.tool_id, 0))?;
        monitoring.call_method1("free_tool_id", (self.tool_id,))?;

        let mut hits = collector.borrow(py).take_hits();
        let mut arcs = collector.borrow(py).take_arcs();
        // Merge subprocess dumps (this process's children, parent or
        // worker alike), then drop the dump dir.
        if let Some(out_dir) = self.child_out_dir.take() {
            if let Ok(entries) = std::fs::read_dir(&out_dir) {
                for entry in entries.filter_map(Result::ok) {
                    let Ok(content) = std::fs::read_to_string(entry.path()) else {
                        continue;
                    };
                    let Ok(dump) = serde_json::from_str::<CovDump>(&content) else {
                        continue;
                    };
                    for (file, lines) in dump.hits {
                        hits.entry(file).or_default().extend(lines);
                    }
                    for (file, file_arcs) in dump.arcs {
                        arcs.entry(file).or_default().extend(file_arcs);
                    }
                }
            }
            let _ = std::fs::remove_dir_all(&out_dir);
            let _ = py
                .import("os")
                .and_then(|os| os.getattr("environ"))
                .and_then(|environ| environ.call_method1("pop", ("PYTEST_RS_COV_OUT", py.None())));
        }
        if ctx.config.is_worker() {
            // Workers don't report: hits and arcs travel to the parent.
            self.dump_payload = Some(
                serde_json::to_string(&CovDump { hits, arcs })
                    .map_err(|e| core_pyo3::exceptions::PyValueError::new_err(e.to_string()))?,
            );
            return Ok(());
        }
        // The parent's own hits (import-time coverage from collection)
        // merge with everything the workers measured.
        for (file, lines) in self.worker_hits.drain() {
            hits.entry(file).or_default().extend(lines);
        }
        for (file, file_arcs) in self.worker_arcs.drain() {
            arcs.entry(file).or_default().extend(file_arcs);
        }
        self.write_data_file(ctx, &hits)?;
        if ctx.config.get_flag("cov-append") {
            self.merge_appended_data(ctx, &mut hits)?;
        }
        let rootdir = ctx
            .config
            .rootdir
            .canonicalize()
            .unwrap_or_else(|_| ctx.config.rootdir.clone());
        let data = self.build_data(&rootdir, hits, arcs);

        let mut messages = Vec::new();
        for spec in &self.reports {
            let (default_dest, content, label) = match spec.kind {
                ReportKind::Xml => (
                    "coverage.xml",
                    report::render_xml(&data, &ctx.config.rootdir.to_string_lossy()),
                    "XML",
                ),
                ReportKind::Lcov => ("coverage.lcov", report::render_lcov(&data), "LCOV"),
                ReportKind::Annotate
                | ReportKind::Html
                | ReportKind::Json
                | ReportKind::Markdown
                | ReportKind::MarkdownAppend => {
                    if let Some(message) =
                        self.coverage_package_report(ctx, spec.kind, spec.dest.as_deref())?
                    {
                        messages.push(message);
                    }
                    continue;
                }
                ReportKind::Term | ReportKind::TermMissing => continue,
            };
            let dest = spec
                .dest
                .clone()
                .unwrap_or_else(|| default_dest.to_string());
            let path = ctx.config.rootdir.join(&dest);
            std::fs::write(&path, content)
                .map_err(|e| core_pyo3::exceptions::PyOSError::new_err(e.to_string()))?;
            messages.push(format!("Coverage {label} written to file {dest}"));
        }
        self.report_messages = messages;

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
        let dump: CovDump = serde_json::from_str(payload)
            .map_err(|e| core_pyo3::exceptions::PyValueError::new_err(e.to_string()))?;
        for (file, lines) in dump.hits {
            self.worker_hits.entry(file).or_default().extend(lines);
        }
        for (file, file_arcs) in dump.arcs {
            self.worker_arcs.entry(file).or_default().extend(file_arcs);
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
        if !self.reports.is_empty() {
            let version_info = ctx.py.import("sys")?.getattr("version_info")?;
            let python_version = format!(
                "{}.{}.{}-{}-{}",
                version_info.getattr("major")?,
                version_info.getattr("minor")?,
                version_info.getattr("micro")?,
                version_info.getattr("releaselevel")?,
                version_info.getattr("serial")?,
            );
            out.push_str(&report::render_header(&python_version));
        }
        if let Some((missing, skip_covered)) = term_spec {
            out.push_str(&report::render_term(data, missing, skip_covered));
        }
        for message in &self.report_messages {
            out.push_str(message);
            out.push('\n');
        }
        if let Some(message) = &self.fail_under_message {
            out.push_str(message);
            out.push('\n');
        }
        Ok(())
    }
}

#[cfg(test)]
mod run_option_tests {
    #[test]
    fn pyproject_branch_true() {
        let dir = std::env::temp_dir().join("ptrs-runopt-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("pyproject.toml"),
            "[tool.coverage.run]\nbranch=true\n",
        )
        .unwrap();
        assert!(super::CovPlugin::run_option_enabled(&dir, None, "branch"));
    }
}
