//! pytest-cov equivalent: Rust-native line coverage via sys.monitoring
//! (PEP 669). Each line costs one callback ever (the callback returns
//! DISABLE), instead of coverage.py's per-line trace overhead.

mod analysis;
mod collector;
mod files;
mod report;

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use collector::{ArcMap, LineCollector};
use files::PathAliases;
use pytest_rs_core::collect::TestItem;
use pytest_rs_core::config::{OptDef, OptionParser};
use pytest_rs_core::hooks::{HookContext, HookResult, Plugin};
use pytest_rs_core::pyo3 as core_pyo3;
use report::{CoverageData, FileRow};

use core_pyo3::prelude::*;

/// Worker -> parent coverage payload (hits and, in branch mode, arcs).
#[derive(serde::Serialize, serde::Deserialize)]
struct CovDump {
    hits: HashMap<String, BTreeSet<u32>>,
    arcs: ArcMap,
    #[serde(default)]
    context_hits: HashMap<String, HashMap<String, BTreeSet<u32>>>,
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
    /// The raw `--cov=X` values `sources` was resolved from, same order —
    /// for the end-of-session "was this source ever measured?" warnings
    /// (coverage.py's `warn_unimported_source`/`_warn_about_unmeasured_code`,
    /// keyed by the original name, not the resolved path).
    cov_source_names: Vec<String>,
    reports: Vec<ReportSpec>,
    fail_under: Option<f64>,
    /// --cov-precision / [report] precision: Cover column decimals.
    precision: usize,
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
    /// coverage [paths] groups (canonical first); serialized to env for the
    /// subprocess shim and used to build each `LineCollector`'s own
    /// `PathAliases` (the remap is applied once per hit, at record time).
    path_aliases: Vec<Vec<String>>,
    /// "Coverage X written to ..." lines for the terminal section.
    report_messages: Vec<String>,
    /// Set instead of `data`/`report_messages` when there was nothing to
    /// report at all (coverage.py's `NoDataError`): printed alone, with
    /// neither the "tests coverage" header nor a (necessarily empty) table.
    report_failed_message: Option<String>,
    /// The sys.monitoring tool id actually claimed (COVERAGE_ID unless a
    /// .pth child collector from an outer session got there first).
    tool_id: u8,
    /// `@pytest.mark.no_cover` (the `no_cover` fixture pauses itself in
    /// Python): events armed before the pause + the subprocess-child hook
    /// path removed from the environment, restored in
    /// `pytest_runtest_teardown`. `None` when the current item isn't paused.
    no_cover_pause: Mutex<Option<(i64, Option<String>)>>,
    /// `--cov-context=test`: switches a "current context" (`{nodeid}|setup`
    /// / `|run` / `|teardown`) at each test-phase boundary, matching
    /// pytest-cov's `TestContextPlugin`. Only `test` is supported (the only
    /// value any conformance test uses; coverage.py's OWN
    /// `dynamic_context=test_function` config is handled separately above).
    context_enabled: bool,
    /// Parent mode: per-context hits merged in from workers, mirroring
    /// `worker_hits` but keyed by context first.
    worker_context_hits: HashMap<String, HashMap<String, BTreeSet<u32>>>,
}

impl CovPlugin {
    pub fn new() -> Self {
        Self {
            enabled: false,
            sources: Vec::new(),
            cov_source_names: Vec::new(),
            reports: Vec::new(),
            fail_under: None,
            precision: 0,
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
            report_failed_message: None,
            tool_id: TOOL_ID,
            no_cover_pause: Mutex::new(None),
            context_enabled: false,
            worker_context_hits: HashMap::new(),
        }
    }

    /// Create this process's own subprocess-coverage dump directory (keyed
    /// on its pid) and point `PYTEST_RS_COV_OUT` at it. Called from
    /// `pytest_configure` (the controller, or a spawned worker running its
    /// own fresh `pytest_configure`) and again from `reinit_post_fork` (a
    /// forked worker, which never re-fires `pytest_configure` at all — see
    /// that method's doc comment).
    fn arm_subprocess_coverage_out_dir(&self, py: Python<'_>) -> PyResult<std::path::PathBuf> {
        let out_dir = std::env::temp_dir().join(format!("pytest-rs-cov-{}", std::process::id()));
        std::fs::create_dir_all(&out_dir)
            .map_err(|e| core_pyo3::exceptions::PyOSError::new_err(e.to_string()))?;
        py.import("os")?
            .getattr("environ")?
            .set_item("PYTEST_RS_COV_OUT", out_dir.to_string_lossy())?;
        Ok(out_dir)
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
                "error: markdown and markdown-append options cannot point to the same file.",
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
            && let Ok(document) = content.parse::<toml::Table>()
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
        // coverage.py keeps the `if TYPE_CHECKING:` default regardless of a
        // custom exclude_lines (it ships as an always-on default), so apply it
        // unconditionally.
        patterns.push(analysis::DEFAULT_EXCLUDE_TYPE_CHECKING.to_string());
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

    /// Resolve a --cov=VALUE entry the way coverage.py does: VALUE is either a
    /// filesystem path or an importable package/module name. We try a rootdir-
    /// relative path first (and the dotted-name-as-subdir spelling), then fall
    /// back to locating the package via the import system — the common case when
    /// the package is installed (editable or not) or in a src/ layout, so its
    /// source does not sit directly under rootdir. Canonicalized so prefix
    /// matching agrees with co_filename (which sees through symlinks like
    /// macOS /tmp).
    fn resolve_source(py: Python<'_>, rootdir: &Path, value: &str) -> PathBuf {
        let as_path = rootdir.join(value);
        if as_path.exists() {
            return as_path.canonicalize().unwrap_or(as_path);
        }
        let as_module = rootdir.join(value.replace('.', "/"));
        if as_module.exists() {
            return as_module.canonicalize().unwrap_or(as_module);
        }
        if let Some(path) = Self::resolve_import_source(py, value) {
            return path.canonicalize().unwrap_or(path);
        }
        as_path
    }

    /// Locate an importable package/module's source via `importlib.util.find_spec`
    /// (a package maps to its directory, a plain module to its `.py` file).
    /// Returns `None` if `value` is not importable in the current environment.
    fn resolve_import_source(py: Python<'_>, value: &str) -> Option<PathBuf> {
        let spec = py
            .import("importlib.util")
            .ok()?
            .call_method1("find_spec", (value,))
            .ok()?;
        if spec.is_none() {
            return None;
        }
        // A package exposes submodule_search_locations; its first entry is the
        // package directory (coverage measures the whole tree under it).
        if let Ok(locs) = spec.getattr("submodule_search_locations")
            && !locs.is_none()
            && let Ok(first) = locs.get_item(0)
            && let Ok(dir) = first.extract::<String>()
        {
            return Some(PathBuf::from(dir));
        }
        // A plain module maps to its source file.
        if let Ok(origin) = spec.getattr("origin")
            && let Ok(path) = origin.extract::<String>()
            && path.ends_with(".py")
        {
            return Some(PathBuf::from(path));
        }
        None
    }

    /// All importable .py files under a source root (for 0%-covered files that
    /// were never imported). Mirrors coverage.py's `find_python_files`: only
    /// basenames matching `^[^.#~!$@%^&*()+=,]+\.pyw?$` count, so dotted or
    /// special-char side-files (`run.local.py`, `foo.bak.py`) — which aren't
    /// importable as modules — are skipped rather than dragging the rate down.
    fn walk_py_files(root: &Path, files: &mut BTreeSet<PathBuf>) {
        static PY_FILE_RE: std::sync::LazyLock<regex::Regex> =
            std::sync::LazyLock::new(|| regex::Regex::new(r"^[^.#~!$@%^&*()+=,]+\.pyw?$").unwrap());
        let is_python_file = |name: &str| PY_FILE_RE.is_match(name) && name != "__pycache__";
        if root.is_file() {
            if root
                .file_name()
                .is_some_and(|n| is_python_file(&n.to_string_lossy()))
            {
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
            } else if is_python_file(&name) {
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
    /// paths under an alias report as the canonical path. Aliases are globs
    /// (coverage.py `PathAliases` semantics: `*` = non-separator run,
    /// `*/dir`/`**/` = any prefix), compiled in `files::PathAliases`.
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
        Self::section_option_value(rootdir, cov_config, "run", option)
    }

    /// One option from a coverage config section ([SECTION] in
    /// .coveragerc/setup.cfg/tox.ini — also the [coverage:SECTION]
    /// spelling — or [tool.coverage.SECTION] in pyproject.toml).
    fn section_option_value(
        rootdir: &Path,
        cov_config: Option<&str>,
        section: &str,
        option: &str,
    ) -> Option<String> {
        let prefixed = format!("[{section}]");
        let spelled = format!("[coverage:{section}]");
        for candidate in [cov_config.unwrap_or(".coveragerc"), "setup.cfg", "tox.ini"] {
            let Ok(content) = std::fs::read_to_string(rootdir.join(candidate)) else {
                continue;
            };
            let mut in_section = false;
            for line in content.lines() {
                let trimmed = line.trim();
                if trimmed.starts_with('[') {
                    in_section = trimmed == prefixed || trimmed == spelled;
                    continue;
                }
                if in_section
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
                .and_then(|coverage| coverage.get(section))
                .and_then(|table| table.get(option))
        {
            return value
                .as_str()
                .map(str::to_string)
                .or_else(|| Some(value.to_string()));
        }
        None
    }

    /// Truthy ini value ("true"/"1"/"yes"/"on", case-insensitive).
    fn truthy(value: &str) -> bool {
        matches!(
            value.to_ascii_lowercase().as_str(),
            "true" | "1" | "yes" | "on"
        )
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
                let configured = Self::section_option_value(
                    &ctx.config.rootdir,
                    ctx.config.get_value("cov-config"),
                    "html",
                    "directory",
                );
                let dir = dest
                    .map(str::to_string)
                    .or(configured)
                    .unwrap_or_else(|| "htmlcov".to_string());
                let dir = dir.as_str();
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
        arcs: &mut ArcMap,
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
                file.clone()
            } else {
                rootdir.join(&file).to_string_lossy().to_string()
            };
            hits.entry(absolute.clone()).or_default().extend(lines);
        }
        // Previous branch runs' arcs come from the sidecar (the data file
        // itself only holds lines).
        let sidecar = Self::arcs_sidecar_path(ctx);
        if let Ok(text) = std::fs::read_to_string(&sidecar)
            && let Ok(prior) = serde_json::from_str::<HashMap<String, Vec<(u32, i64, u8)>>>(&text)
        {
            for (file, file_arcs) in prior {
                arcs.entry(file).or_default().extend(file_arcs);
            }
        }
        Ok(())
    }

    /// coverage.py `warn_unimported_source`/`_warn_about_unmeasured_code`: a
    /// `--cov=X` source that was never actually traced — either because it
    /// was imported (by a `-p` plugin, a conftest, ...) before this
    /// process ever started tracing, or never imported at all. Returns the
    /// unmeasured sources' resolved paths: coverage.py's own report-file
    /// selection is driven by what was actually observed (`measured_files`),
    /// not by walking `[run] source` for candidates, so an unmeasured
    /// source contributes no row at all (not even a 0% one) — unlike a
    /// measured-but-incomplete source, which still gets the usual
    /// zero-coverage-line treatment for its untouched files.
    fn warn_unmeasured_sources(
        &self,
        py: Python<'_>,
        hits: &HashMap<String, BTreeSet<u32>>,
    ) -> PyResult<Vec<PathBuf>> {
        let sys_modules = py.import("sys")?.getattr("modules")?;
        let mut unmeasured = Vec::new();
        for (name, source) in self.cov_source_names.iter().zip(self.sources.iter()) {
            let source_str = source.to_string_lossy();
            let measured = hits
                .keys()
                .any(|file| file == source_str.as_ref() || file.starts_with(source_str.as_ref()));
            if measured {
                continue;
            }
            unmeasured.push(source.clone());
            let module = sys_modules.call_method1("get", (name, py.None()))?;
            if module.is_none() {
                Self::emit_warning(
                    py,
                    "CoverageWarning",
                    &format!("Module {name} was never imported. (module-not-imported)"),
                )?;
                continue;
            }
            // A namespace package (multiple __path__ entries) has no code of
            // its own to measure — not a warning-worthy gap.
            if module
                .getattr("__path__")
                .and_then(|path| path.len())
                .is_ok_and(|len| len > 1)
            {
                continue;
            }
            if module
                .getattr("__file__")
                .map(|file| file.is_none())
                .unwrap_or(true)
            {
                Self::emit_warning(
                    py,
                    "CoverageWarning",
                    &format!("Module {name} has no Python source. (module-not-python)"),
                )?;
                continue;
            }
            Self::emit_warning(
                py,
                "CoverageWarning",
                &format!(
                    "Module {name} was previously imported, but not measured \
                     (module-not-measured)"
                ),
            )?;
        }
        Ok(unmeasured)
    }

    /// A warning that must actually reach the terminal (bypassing
    /// `filterwarnings = error`) — mirrors real pytest-cov registering a
    /// `once` filter for its own warning categories ahead of the ini's
    /// blanket rule, then letting the warning fall through to the real
    /// `showwarning` (capture is briefly lifted so it prints immediately
    /// instead of landing in the end-of-session warnings summary, matching
    /// coverage.py emitting these directly to stderr as they happen).
    fn emit_warning(py: Python<'_>, category_name: &str, message: &str) -> PyResult<()> {
        let category = py.import("pytest_cov")?.getattr(category_name)?;
        let warnings = py.import("warnings")?;
        warnings.call_method1("simplefilter", ("once", &category))?;
        let wcapture = py.import("pytest._wcapture")?;
        wcapture.call_method0("uninstall")?;
        let instance = category.call1((message,))?;
        let result = warnings.call_method1("warn", (instance,)).map(|_| ());
        wcapture.call_method0("install")?;
        result
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
        arcs: &ArcMap,
        context_hits: &HashMap<String, HashMap<String, BTreeSet<u32>>>,
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
        let relative_key = |file: &str| -> String {
            if relative {
                Path::new(file)
                    .strip_prefix(&rootdir_canon)
                    .map(|p| p.to_string_lossy().to_string())
                    .unwrap_or_else(|_| file.to_string())
            } else {
                file.to_string()
            }
        };
        // --cov-context=test: one add_lines call per context (coverage.py's
        // CoverageData.set_context + add_lines pattern) instead of a single
        // blanket write — context_hits' union across contexts equals `hits`
        // exactly (every hit is recorded under whichever context, including
        // `""`, was current when it fired), so this replaces rather than
        // supplements the blanket write.
        if context_hits.is_empty() {
            let lines = core_pyo3::types::PyDict::new(py);
            for (file, hit_lines) in hits {
                let list: Vec<u32> = hit_lines.iter().copied().collect();
                lines.set_item(relative_key(file), list)?;
            }
            data.call_method1("add_lines", (lines,))?;
        } else {
            for (context, file_hits) in context_hits {
                let lines = core_pyo3::types::PyDict::new(py);
                for (file, hit_lines) in file_hits {
                    let list: Vec<u32> = hit_lines.iter().copied().collect();
                    lines.set_item(relative_key(file), list)?;
                }
                data.call_method1("set_context", (context.as_str(),))?;
                data.call_method1("add_lines", (lines,))?;
            }
        }
        data.call_method0("write")?;
        // Branch arcs use an internal representation (src, dst, direction)
        // that coverage.py's data model cannot hold; a sidecar JSON next to
        // the data file lets --cov-append restore them.
        let sidecar = Self::arcs_sidecar_path(ctx);
        if arcs.values().any(|file_arcs| !file_arcs.is_empty()) {
            let serializable: HashMap<&String, Vec<(u32, i64, u8)>> = arcs
                .iter()
                .map(|(file, file_arcs)| (file, file_arcs.iter().copied().collect()))
                .collect();
            if let Ok(text) = serde_json::to_string(&serializable) {
                let _ = std::fs::write(&sidecar, text);
            }
        } else if !append {
            let _ = std::fs::remove_file(&sidecar);
        }
        Ok(())
    }

    /// The branch-arcs sidecar next to the `.coverage` data file.
    fn arcs_sidecar_path(ctx: &HookContext) -> PathBuf {
        let mut path = Self::data_file_path(ctx).into_os_string();
        path.push(".pytest-rs-arcs");
        PathBuf::from(path)
    }

    fn build_data(
        &self,
        rootdir: &Path,
        hits: HashMap<String, BTreeSet<u32>>,
        arcs: ArcMap,
        unmeasured_sources: &[PathBuf],
    ) -> CoverageData {
        // Report set: every hit file, plus (with explicit --cov=src) every
        // .py file under the sources, so never-imported files show as 0% —
        // except a source that was never measured at all (imported before
        // tracing started, or never imported), which coverage.py excludes
        // entirely rather than showing as a 0% row.
        let mut files: BTreeSet<PathBuf> = hits.keys().map(PathBuf::from).collect();
        for source in &self.sources {
            if unmeasured_sources.contains(source) {
                continue;
            }
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
            // A file the analyzer found statement-free (an empty module, or one
            // with only comments / a docstring / `...` stubs) has zero
            // statements — coverage.py reports it as 0/0 = 100%. Don't let an
            // import-time phantom LINE event (e.g. an empty module's RESUME)
            // invent a statement via the union below.
            let covered: BTreeSet<u32> = if analysis.executable.is_empty() {
                BTreeSet::new()
            } else {
                hits.map(|lines| {
                    lines
                        .iter()
                        .map(|line| fold(*line))
                        .filter(|line| !analysis.excluded.contains(line))
                        .collect()
                })
                .unwrap_or_default()
            };
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
            // `path` is already canonical here: [paths] aliasing (e.g. a
            // worker's chdir copy matched by `*/dir1`) is applied once, when
            // the hit/arc is first recorded (collector.rs's `canonical_name`,
            // `_child.py`'s `_map_alias`), so every producer's key already
            // collapses onto the same canonical name before it ever reaches
            // this per-file loop.
            let name = path
                .strip_prefix(rootdir)
                .unwrap_or(&path)
                .to_string_lossy()
                .to_string();
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

    /// `--cov-context=test`: mirrors pytest-cov's `TestContextPlugin.
    /// switch_context` (`f'{item.nodeid}|{when}'`), a no-op unless
    /// `--cov-context=test` is active.
    fn switch_context(&self, py: Python<'_>, item: &TestItem, when: &str) {
        if self.context_enabled
            && let Some(collector) = &self.collector
        {
            collector
                .borrow(py)
                .set_context(&format!("{}|{when}", item.nodeid));
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

    fn reinit_post_fork(&mut self, py: Python<'_>) {
        // Only relevant once `pytest_configure` has actually run (coverage
        // enabled at all) and set up a dump dir to begin with.
        if self.child_out_dir.is_none() {
            return;
        }
        // The inherited out_dir is the *controller's* pid-named directory —
        // every forked sibling would otherwise point its own subprocess
        // children (via the inherited PYTEST_RS_COV_OUT env var) at that
        // one shared, controller-owned directory. Two siblings racing to
        // read-then-delete it in their own pytest_sessionfinish (see that
        // method below) is exactly how one sibling's subprocess coverage
        // dump silently vanishes: whichever sessionfinish runs first reads
        // (possibly) both dumps and removes the directory out from under
        // the other. Point this worker at its own, freshly pid-named dir.
        if let Ok(out_dir) = self.arm_subprocess_coverage_out_dir(py) {
            self.child_out_dir = Some(out_dir);
        }
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
        parser.add_option(OptDef::value(
            "--cov-precision",
            None,
            "override the reporting precision (decimals in the Cover column)",
        ));
        parser.add_option(OptDef::flag(
            "--cov-reset",
            "accepted but inert: resets preceding --cov options (positional \
             option order is not tracked; later --cov values still apply)",
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
            let path = package_root.join(rel);
            let needs_write = std::fs::read(&path)
                .map(|existing| existing != content.as_bytes())
                .unwrap_or(true);
            if needs_write {
                std::fs::write(&path, content)
                    .map_err(|e| core_pyo3::exceptions::PyOSError::new_err(e.to_string()))?;
            }
        }

        // Register cov/no_cover fixtures unconditionally — test_funcarg_not_active
        // runs without --cov but still needs the cov fixture (returning None).
        {
            let plugin_module = py.import("pytest_cov.plugin")?;
            pytest_rs_core::python::register_plugin_fixtures(
                py,
                &plugin_module,
                &mut ctx.session.registry,
            )?;
        }

        let Some(mut cov_values) = ctx
            .config
            .get_values("cov")
            .map(|values| values.iter().map(|v| v.to_string()).collect::<Vec<_>>())
        else {
            return Ok(());
        };
        // --cov-reset clears the --cov options seen so far; option order
        // matters, so rescan argv when it appears.
        if ctx.config.get_flag("cov-reset") {
            let mut rescanned: Vec<String> = Vec::new();
            for arg in &ctx.config.effective_args {
                if arg == "--cov" {
                    rescanned.push(String::new());
                } else if let Some(value) = arg.strip_prefix("--cov=") {
                    rescanned.push(value.to_string());
                } else if arg == "--cov-reset" {
                    rescanned.clear();
                }
            }
            if rescanned.is_empty() {
                return Ok(());
            }
            cov_values = rescanned;
        }
        if ctx.config.get_flag("no-cov") {
            // Cov options appearing AFTER --no-cov: tell the user, like
            // pytest-cov (a printed line plus a warning). Options given
            // before --no-cov (e.g. --cov --no-cov) stay silent.
            let args = &ctx.config.effective_args;
            let after_no_cov = args
                .iter()
                .rposition(|arg| arg == "--no-cov")
                .map(|pos| &args[pos + 1..])
                .unwrap_or(&[]);
            if after_no_cov
                .iter()
                .any(|arg| arg == "--cov" || arg.starts_with("--cov=") || arg.starts_with("--cov-"))
            {
                let message = "Coverage disabled via --no-cov switch!";
                println!("WARNING: {message}");
                let _ = pytest_rs_core::python::warn_explicit_at(
                    py,
                    "PytestWarning",
                    message,
                    "pytest_cov/plugin.py",
                    0,
                );
            }
            return Ok(());
        }
        self.enabled = true;
        let rootdir = ctx
            .config
            .rootdir
            .canonicalize()
            .unwrap_or_else(|_| ctx.config.rootdir.clone());
        self.cov_source_names = cov_values
            .iter()
            .filter(|value| !value.is_empty())
            .cloned()
            .collect();
        self.sources = self
            .cov_source_names
            .iter()
            .map(|value| Self::resolve_source(py, &rootdir, value))
            .collect();
        self.reports = Self::parse_reports(py, ctx.config.get_values("cov-report"))?;
        let cov_config = ctx.config.get_value("cov-config");
        let plain_rootdir = ctx.config.rootdir.clone();
        // [report] show_missing / skip_covered upgrade the term spec like
        // the --cov-report=term-missing:skip-covered spellings.
        if Self::section_option_value(&plain_rootdir, cov_config, "report", "show_missing")
            .is_some_and(|value| Self::truthy(&value))
        {
            for spec in &mut self.reports {
                if matches!(spec.kind, ReportKind::Term) {
                    spec.kind = ReportKind::TermMissing;
                }
            }
        }
        if Self::section_option_value(&plain_rootdir, cov_config, "report", "skip_covered")
            .is_some_and(|value| Self::truthy(&value))
        {
            for spec in &mut self.reports {
                if matches!(spec.kind, ReportKind::Term | ReportKind::TermMissing) {
                    spec.skip_covered = true;
                }
            }
        }
        self.precision = ctx
            .config
            .get_value("cov-precision")
            .map(str::to_string)
            .or_else(|| {
                Self::section_option_value(&plain_rootdir, cov_config, "report", "precision")
            })
            .and_then(|value| value.parse().ok())
            .unwrap_or(0);
        // [report] fail_under: --cov-fail-under wins; otherwise .coveragerc
        // / setup.cfg / tox.ini [report] or pyproject [tool.coverage.report].
        self.fail_under = ctx
            .config
            .get_value("cov-fail-under")
            .and_then(|value| value.parse().ok())
            .or_else(|| {
                Self::section_option_value(&plain_rootdir, cov_config, "report", "fail_under")
                    .and_then(|value| value.parse().ok())
            });
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
        self.context_enabled = ctx.config.get_value("cov-context") == Some("test");
        self.path_aliases =
            Self::paths_aliases(&ctx.config.rootdir, ctx.config.get_value("cov-config"));
        // An xdist worker runs with a rootdir set to its chdir; the coverage
        // config (rsync'd as a fixture) may not sit on its rootdir resolution
        // path, so `[paths]` parses empty. Fall back to the controller's
        // serialized aliases so worker-native tracing and the subprocess shim
        // still accept files under an alias (`*/dir1`).
        if self.path_aliases.is_empty()
            && ctx.config.is_worker()
            && let Ok(raw) = std::env::var("PYTEST_RS_COV_PATHS")
            && !raw.is_empty()
            && let Ok(groups) = serde_json::from_str::<Vec<Vec<String>>>(&raw)
        {
            self.path_aliases = groups;
        }

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
                PathAliases::from_groups(&self.path_aliases, &rootdir),
                with_sep(&pytest_rs_core::python::shim_root()),
                self.branch,
                need_jump_targets,
                local_events,
                disable,
                monitoring.clone().unbind(),
                self.tool_id,
                self.context_enabled,
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
        self.collector = Some(collector);

        // Subprocess coverage: python children self-measure via the site
        // .pth hook (a no-op without these env vars) and dump for merging.
        let out_dir = self.arm_subprocess_coverage_out_dir(py)?;
        let environ = py.import("os")?.getattr("environ")?;
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
        // [run] sigterm = true: a subprocess that gets SIGTERM'd (proc.terminate())
        // has no atexit chance to dump — the child shim installs its own
        // SIGTERM handler to save data first, then re-raises SIG_DFL.
        environ.set_item(
            "PYTEST_RS_COV_SIGTERM",
            if Self::run_option_enabled(
                &ctx.config.rootdir,
                ctx.config.get_value("cov-config"),
                "sigterm",
            ) {
                "1"
            } else {
                "0"
            },
        )?;
        environ.set_item("PYTEST_RS_COV_TOOL_ID", self.tool_id.to_string())?;
        environ.set_item("PYTEST_RS_COV_ACTIVE", "1")?;
        // [paths] aliases serialized as JSON (canonical first per group) so
        // the subprocess shim can accept files under an alias (`*/dir1`) the
        // same way the native LineCollector does.
        if let Ok(paths_json) = serde_json::to_string(&self.path_aliases) {
            environ.set_item("PYTEST_RS_COV_PATHS", paths_json)?;
        }
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
        // Children spawned via sys.executable run the VIRTUAL_ENV python,
        // which only processes its own site-packages .pth files.
        if let Ok(venv) = std::env::var("VIRTUAL_ENV") {
            let venv = Path::new(&venv);
            let mut site_dirs: Vec<std::path::PathBuf> = Vec::new();
            if let Ok(entries) = std::fs::read_dir(venv.join("lib")) {
                for entry in entries.filter_map(Result::ok) {
                    let dir = entry.path().join("site-packages");
                    if dir.is_dir() {
                        site_dirs.push(dir);
                    }
                }
            }
            let windows_dir = venv.join("Lib").join("site-packages");
            if windows_dir.is_dir() {
                site_dirs.push(windows_dir);
            }
            for dir in site_dirs {
                let _ = std::fs::write(dir.join("pytest-rs-cov.pth"), PTH_LINE);
            }
        }
        Ok(())
    }

    /// Arms tracing only once `-p`/entry-point plugins have imported —
    /// matching real pytest-cov, whose coverage-start hook
    /// (`pytest_load_initial_conftests`) runs after upstream's own early
    /// `-p` plugin loading. A plugin loaded via `-p` that imports a
    /// `--cov`-measured module at its own import time (module-level code)
    /// must not have that import counted (coverage.py would report it as
    /// "previously imported, but not measured").
    fn pytest_plugins_registered(&mut self, ctx: &mut HookContext) -> PyResult<()> {
        if self.collector.is_none() {
            return Ok(());
        }
        let py = ctx.py;
        let monitoring = py.import("sys")?.getattr("monitoring")?;
        let py_start_event = monitoring.getattr("events")?.getattr("PY_START")?;
        // Globally only the PY_START gate; LINE events arm per tracked code
        // object (coverage.py's sysmon core layout).
        monitoring.call_method1("set_events", (self.tool_id, &py_start_event))?;
        monitoring.call_method0("restart_events")?;
        Ok(())
    }

    /// `@pytest.mark.no_cover`: pytest-cov's `pytest_runtest_call`
    /// hookwrapper pauses the collector around just the call phase; a
    /// native plugin has no hookwrapper slot there, so this brackets
    /// setup-end..teardown-start instead (equivalent for a marker with no
    /// fixtures of its own, which is the only shape upstream's own marker
    /// form takes — fixture-based pausing already happens in the `no_cover`
    /// fixture itself, in `py/pytest_cov/plugin.py`).
    fn pytest_runtest_setup(&self, ctx: &mut HookContext, item: &TestItem) -> PyResult<()> {
        if !self.enabled {
            return Ok(());
        }
        self.switch_context(ctx.py, item, "setup");
        if item.get_closest_marker("no_cover").is_none() {
            return Ok(());
        }
        let py = ctx.py;
        let monitoring = py.import("sys")?.getattr("monitoring")?;
        let events: i64 = monitoring
            .call_method1("get_events", (self.tool_id,))?
            .extract()?;
        monitoring.call_method1("set_events", (self.tool_id, 0))?;
        let environ = py.import("os")?.getattr("environ")?;
        let saved_child: Option<String> = environ
            .call_method1("pop", ("PYTEST_RS_COV_CHILD", py.None()))?
            .extract()
            .ok();
        *self.no_cover_pause.lock().expect("cov lock poisoned") = Some((events, saved_child));
        Ok(())
    }

    fn pytest_runtest_teardown(&self, ctx: &mut HookContext, item: &TestItem) -> PyResult<()> {
        if self.enabled {
            self.switch_context(ctx.py, item, "teardown");
        }
        let Some((events, saved_child)) = self
            .no_cover_pause
            .lock()
            .expect("cov lock poisoned")
            .take()
        else {
            return Ok(());
        };
        let py = ctx.py;
        py.import("sys")?
            .getattr("monitoring")?
            .call_method1("set_events", (self.tool_id, events))?;
        if let Some(child) = saved_child {
            py.import("os")?
                .getattr("environ")?
                .set_item("PYTEST_RS_COV_CHILD", child)?;
        }
        Ok(())
    }

    /// `pytest_pyfunc_call` fires (as an observer only — never claims the
    /// invocation) right before the test's own body runs, so it is the
    /// closest native equivalent to upstream's `pytest_runtest_call`
    /// hookwrapper for switching into the "run" context.
    fn pytest_pyfunc_call(
        &self,
        ctx: &mut HookContext,
        item: &TestItem,
        _callable: &Py<PyAny>,
        _kwargs: &[(String, Py<PyAny>)],
    ) -> HookResult<()> {
        if self.enabled {
            self.switch_context(ctx.py, item, "run");
        }
        Ok(None)
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
        let mut context_hits = collector.borrow(py).take_context_hits();
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
                    for (context, file_hits) in dump.context_hits {
                        let entry = context_hits.entry(context).or_default();
                        for (file, lines) in file_hits {
                            entry.entry(file).or_default().extend(lines);
                        }
                    }
                }
            }
            let _ = std::fs::remove_dir_all(&out_dir);
            let _ = py
                .import("os")
                .and_then(|os| os.getattr("environ"))
                .and_then(|environ| {
                    environ.call_method1("pop", ("PYTEST_RS_COV_OUT", py.None()))?;
                    environ.call_method1("pop", ("PYTEST_RS_COV_PATHS", py.None()))
                });
        }
        if ctx.config.is_worker() {
            // Workers don't report: hits and arcs travel to the parent.
            self.dump_payload = Some(
                serde_json::to_string(&CovDump {
                    hits,
                    arcs,
                    context_hits,
                })
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
        for (context, file_hits) in self.worker_context_hits.drain() {
            let entry = context_hits.entry(context).or_default();
            for (file, lines) in file_hits {
                entry.entry(file).or_default().extend(lines);
            }
        }
        if ctx.config.get_flag("cov-append") {
            self.merge_appended_data(ctx, &mut hits, &mut arcs)?;
        }
        let unmeasured_sources = self.warn_unmeasured_sources(py, &hits)?;
        if hits.is_empty() {
            Self::emit_warning(
                py,
                "CoverageWarning",
                "No data was collected. (no-data-collected)",
            )?;
        }
        self.write_data_file(ctx, &hits, &arcs, &context_hits)?;
        let rootdir = ctx
            .config
            .rootdir
            .canonicalize()
            .unwrap_or_else(|_| ctx.config.rootdir.clone());
        let data = self.build_data(&rootdir, hits, arcs, &unmeasured_sources);

        // coverage.py's report_core.get_analysis_to_report: an empty file
        // set (nothing matched any `--cov` source at all) fails every
        // report kind at once with NoDataError("No data to report."),
        // which pytest-cov converts to a warning instead of writing
        // anything.
        if data.rows.is_empty() && !self.reports.is_empty() {
            let message = "Failed to generate report: No data to report.".to_string();
            Self::emit_warning(py, "CovReportWarning", &message)?;
            self.report_messages = Vec::new();
            self.report_failed_message = Some(message);
            return Ok(());
        }

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
        for (context, file_hits) in dump.context_hits {
            let entry = self.worker_context_hits.entry(context).or_default();
            for (file, lines) in file_hits {
                entry.entry(file).or_default().extend(lines);
            }
        }
        Ok(())
    }

    fn pytest_terminal_summary(&self, ctx: &mut HookContext, out: &mut String) -> PyResult<()> {
        if let Some(message) = &self.report_failed_message {
            out.push_str(&format!("\nWARNING: {message}\n"));
            return Ok(());
        }
        let Some(data) = &self.data else {
            return Ok(());
        };
        // --no-cov-on-fail: a failing session prints no coverage at all.
        if ctx.config.get_flag("no-cov-on-fail")
            && ctx
                .session
                .reports
                .iter()
                .any(|report| report.outcome == pytest_rs_core::report::Outcome::Failed)
        {
            return Ok(());
        }
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
            out.push_str(&report::render_term(
                data,
                missing,
                skip_covered,
                self.precision,
            ));
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

    #[test]
    fn walk_py_files_skips_dotted_sidefiles() {
        use std::collections::BTreeSet;
        let dir = std::env::temp_dir().join("ptrs-walkpy-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for name in [
            "__init__.py",
            "normal.py",
            "run.local.py",
            "foo.bak.py",
            "mod.pyw",
        ] {
            std::fs::write(dir.join(name), "X = 1\n").unwrap();
        }
        let mut files = BTreeSet::new();
        super::CovPlugin::walk_py_files(&dir, &mut files);
        let names: BTreeSet<String> = files
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        // Importable module names only — dotted side-files are excluded,
        // matching coverage.py's find_python_files filter.
        assert!(names.contains("__init__.py"));
        assert!(names.contains("normal.py"));
        assert!(names.contains("mod.pyw"));
        assert!(!names.contains("run.local.py"));
        assert!(!names.contains("foo.bak.py"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pyproject_exclude_lines() {
        let dir = std::env::temp_dir().join("ptrs-exclude-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("pyproject.toml"),
            "[tool.coverage.report]\nexclude_lines = [\n    'raise NotImplementedError',\n]\n",
        )
        .unwrap();
        let patterns = super::CovPlugin::load_exclude_patterns(&dir, None);
        assert!(
            patterns
                .iter()
                .any(|re| re.is_match("    raise NotImplementedError")),
            "{patterns:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
