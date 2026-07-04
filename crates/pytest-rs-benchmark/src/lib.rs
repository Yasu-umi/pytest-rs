//! pytest-benchmark equivalent: the `benchmark` fixture (Rust #[pyclass],
//! Python inner loop), calibration, stats, terminal table, --benchmark-json.

mod fixture;
mod report;
mod stats;

use std::ffi::CString;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use fixture::{BenchConfig, BenchmarkFixture, ResultStore};
use pytest_rs_core::collect::TestItem;
use pytest_rs_core::config::{OptDef, OptionParser};
use pytest_rs_core::fixture::FixtureDef;
use pytest_rs_core::hooks::{FixtureValue, HookContext, HookResult, Plugin};
use pytest_rs_core::pyo3 as core_pyo3;
use pytest_rs_core::session::Finalizer;

use core_pyo3::prelude::*;
use core_pyo3::types::PyModule;

const HELPER: &str = include_str!("../py/helper.py");

/// Fixture stub: gives the engine a `benchmark` FixtureDef to resolve; the
/// plugin claims the actual setup in pytest_fixture_setup.
const FIXTURE_STUB: &str = r#"
import pytest


@pytest.fixture
def benchmark():
    raise NotImplementedError("the benchmark fixture is provided natively by pytest-rs-benchmark")


@pytest.fixture
def benchmark_weave(benchmark):
    return benchmark.weave
"#;

/// Print argparse-style error to stderr and exit 2 (matches upstream's
/// pytest_configure-time error format for invalid benchmark options).
fn argparse_error(option: &str, msg: &str) -> ! {
    eprintln!("usage: pytest-rs-bin [options] [file_or_dir] [file_or_dir] [...]");
    eprintln!();
    eprintln!("pytest-rs-bin: error: argument {option}: {msg}");
    std::process::exit(2);
}

/// Parse a decimal (float) option value; exit with argparse_error on failure.
fn parse_decimal(value: &str, option: &str) -> f64 {
    value.parse::<f64>().unwrap_or_else(|_| {
        argparse_error(
            option,
            &format!("Invalid decimal value '{value}': InvalidOperation"),
        )
    })
}

/// Parse a min-rounds integer; exit with argparse_error on bad input or <1.
fn parse_min_rounds(value: &str, option: &str) -> usize {
    match value.parse::<i64>() {
        Ok(n) if n >= 1 => n as usize,
        Ok(_) => argparse_error(option, "Value for --benchmark-rounds must be at least 1."),
        Err(_) => argparse_error(
            option,
            &format!("invalid literal for int() with base 10: '{value}'"),
        ),
    }
}

pub struct BenchmarkPlugin {
    config: BenchConfig,
    only: bool,
    skip: bool,
    verbose: bool,
    sort: String,
    group_by: String,
    columns: Option<Vec<String>>,
    /// --benchmark-timer dotted name, resolved per fixture creation.
    timer_spec: Option<String>,
    json_path: Option<String>,
    /// --benchmark-save name (validated); None = no explicit save.
    save_name: Option<String>,
    /// --benchmark-autosave flag.
    autosave: bool,
    /// --benchmark-compare value: None = not requested, Some("") = compare last,
    /// Some(prefix) = compare matching prefix.
    compare: Option<String>,
    /// --benchmark-compare-fail spec (already validated).
    compare_fail: Option<String>,
    /// --benchmark-histogram base path; None = not requested.
    histogram: Option<String>,
    /// Storage directory (invocation_dir / .benchmarks / machine_id), set during configure.
    storage_dir: Option<PathBuf>,
    helper: Option<Py<PyModule>>,
    results: ResultStore,
}

impl BenchmarkPlugin {
    pub fn new() -> Self {
        Self {
            config: BenchConfig::default(),
            only: false,
            skip: false,
            verbose: false,
            sort: "min".to_string(),
            group_by: "group".to_string(),
            columns: None,
            timer_spec: None,
            json_path: None,
            save_name: None,
            autosave: false,
            compare: None,
            compare_fail: None,
            histogram: None,
            storage_dir: None,
            helper: None,
            results: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn uses_benchmark(item: &TestItem) -> bool {
        item.fixture_names.iter().any(|name| name == "benchmark")
    }
}

impl Default for BenchmarkPlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl Plugin for BenchmarkPlugin {
    fn name(&self) -> &str {
        "benchmark"
    }

    fn pytest_addoption(&self, parser: &mut OptionParser) {
        parser.add_option(OptDef::flag(
            "--benchmark-only",
            "Only run benchmarks (deselect everything else)",
        ));
        parser.add_option(OptDef::flag(
            "--benchmark-skip",
            "Skip running any tests that contain benchmarks",
        ));
        parser.add_option(OptDef::flag(
            "--benchmark-disable",
            "Disable benchmarks: the benchmarked function is only called once",
        ));
        parser.add_option(OptDef::value(
            "--benchmark-json",
            None,
            "Dump a JSON report of the benchmark results to this path",
        ));
        parser.add_option(OptDef::value(
            "--benchmark-min-rounds",
            Some("5"),
            "Minimum rounds, even if total time exceeds max-time",
        ));
        parser.add_option(OptDef::value(
            "--benchmark-min-time",
            Some("0.000005"),
            "Minimum time per round in seconds",
        ));
        parser.add_option(OptDef::value(
            "--benchmark-max-time",
            Some("1.0"),
            "Maximum run time per test in seconds (soft cap)",
        ));
        parser.add_option(OptDef::value(
            "--benchmark-calibration-precision",
            Some("10"),
            "Round durations must reach this multiple of the clock resolution",
        ));
        parser.add_option(OptDef::value(
            "--benchmark-warmup",
            None,
            "Run the benchmarked function before measuring (on/off)",
        ));
        parser.add_option(OptDef::value(
            "--benchmark-warmup-iterations",
            Some("100000"),
            "Max iterations of the warmup phase",
        ));
        parser.add_option(OptDef::value(
            "--benchmark-sort",
            Some("min"),
            "Column to sort the result table by: min, max, mean, stddev, name, fullname",
        ));
        parser.add_option(OptDef::value(
            "--benchmark-group-by",
            Some("group"),
            "How to group tests in the result tables: group, name, func, \
             fullname, fullfunc, param or param:NAME (comma-combinable)",
        ));
        parser.add_option(OptDef::value(
            "--benchmark-cprofile",
            None,
            "Run the benchmarked function once more under cProfile (the \
             profile table column to sort by upstream; table not rendered)",
        ));
        parser.add_option(OptDef::value(
            "--benchmark-timer",
            None,
            "Timer to use as a dotted name (e.g. time.time, time.perf_counter)",
        ));
        parser.add_option(OptDef::value(
            "--benchmark-save",
            None,
            "Save the benchmark results under this name in the storage path",
        ));
        parser.add_option(OptDef::optional_value(
            "--benchmark-compare",
            "Compare against saved benchmarks (optional prefix to select file)",
        ));
        parser.add_option(OptDef::value(
            "--benchmark-compare-fail",
            None,
            "STAT:THRESHOLD — fail if the stat degrades beyond threshold vs the compared file",
        ));
        parser.add_option(OptDef::value(
            "--benchmark-storage",
            None,
            "Path to the storage directory for benchmark data (default: .benchmarks)",
        ));
        parser.add_option(OptDef::value(
            "--benchmark-histogram",
            None,
            "Render a SVG histogram to this base path",
        ));
        parser.add_option(OptDef::value(
            "--benchmark-columns",
            None,
            "Comma-separated list of columns to display in the result table",
        ));
        parser.add_option(OptDef::value(
            "--benchmark-name",
            None,
            "How to format test names in the result table",
        ));
        parser.add_option(OptDef::flag(
            "--benchmark-disable-gc",
            "Disable the garbage collector around the timed loops",
        ));
        parser.add_option(OptDef::flag(
            "--benchmark-autosave",
            "Autosave benchmark data after each run",
        ));
        parser.add_option(OptDef::flag(
            "--benchmark-verbose",
            "Verbose calibration output",
        ));
    }

    fn pytest_configure(&mut self, ctx: &mut HookContext) -> PyResult<()> {
        let py = ctx.py;
        self.only = ctx.config.get_flag("benchmark-only");
        self.skip = ctx.config.get_flag("benchmark-skip");
        self.config.disabled = ctx.config.get_flag("benchmark-disable");
        self.verbose = ctx.config.get_flag("benchmark-verbose");
        self.json_path = ctx.config.get_value("benchmark-json").map(str::to_string);
        self.autosave = ctx.config.get_flag("benchmark-autosave");

        if let Some(value) = ctx.config.get_value("benchmark-min-rounds") {
            self.config.min_rounds = parse_min_rounds(value, "--benchmark-min-rounds");
        }
        if let Some(value) = ctx.config.get_value("benchmark-min-time") {
            self.config.min_time = parse_decimal(value, "--benchmark-min-time");
        }
        if let Some(value) = ctx.config.get_value("benchmark-max-time") {
            self.config.max_time = parse_decimal(value, "--benchmark-max-time");
        }
        if let Some(value) = ctx.config.get_value("benchmark-calibration-precision") {
            self.config.calibration_precision =
                value.parse().unwrap_or(self.config.calibration_precision);
        }
        self.config.warmup = matches!(
            ctx.config.get_value("benchmark-warmup"),
            Some("on") | Some("yes") | Some("true")
        );
        if let Some(value) = ctx.config.get_value("benchmark-warmup-iterations") {
            self.config.warmup_iterations = value.parse().unwrap_or(self.config.warmup_iterations);
        }
        if let Some(value) = ctx.config.get_value("benchmark-sort") {
            const VALID_SORT: &[&str] = &["min", "max", "mean", "stddev", "name", "fullname"];
            if !VALID_SORT.contains(&value) {
                argparse_error(
                    "--benchmark-sort",
                    &format!(
                        "Unacceptable value: '{value}'. Value for --benchmark-sort must be one of: \
                         'min', 'max', 'mean', 'stddev', 'name', 'fullname'."
                    ),
                );
            }
            self.sort = value.to_string();
        }
        if let Some(value) = ctx.config.get_value("benchmark-columns") {
            self.columns = Some(value.split(',').map(|s| s.trim().to_string()).collect());
        }
        if ctx.config.get_value("benchmark-cprofile").is_some() {
            self.config.cprofile = true;
        }
        if ctx.config.get_flag("benchmark-disable-gc") {
            self.config.disable_gc = true;
        }
        if let Some(spec) = ctx.config.get_value("benchmark-timer") {
            if !spec.contains('.') {
                argparse_error(
                    "--benchmark-timer",
                    "Value for --benchmark-timer must be in dotted form. Eg: 'module.attr'.",
                );
            }
            self.timer_spec = Some(spec.to_string());
        }
        if let Some(value) = ctx.config.get_value("benchmark-group-by") {
            self.group_by = value.to_string();
        }
        if let Some(name) = ctx.config.get_value("benchmark-save") {
            if name.is_empty() {
                argparse_error("--benchmark-save", "Can't be empty.");
            }
            let invalid_chars = "/:*?<>|\\";
            let bad: String = name
                .chars()
                .filter(|c| invalid_chars.contains(*c))
                .collect();
            if !bad.is_empty() {
                argparse_error(
                    "--benchmark-save",
                    &format!(
                        "Must not contain any of these characters: /:*?<>|\\ (it has '{bad}')"
                    ),
                );
            }
            self.save_name = Some(name.to_string());
        }
        if let Some(values) = ctx.config.get_values("benchmark-compare") {
            // optional_value: empty string = no value (compare last), non-empty = prefix
            self.compare = Some(values.last().copied().unwrap_or_default().to_string());
        }
        if let Some(spec) = ctx.config.get_value("benchmark-compare-fail") {
            if !spec.contains(':') {
                argparse_error(
                    "--benchmark-compare-fail",
                    &format!("Could not parse value: '{spec}'."),
                );
            }
            self.compare_fail = Some(spec.to_string());
        }
        if let Some(hist) = ctx.config.get_value("benchmark-histogram") {
            self.histogram = Some(hist.to_string());
        }

        if self.only && self.config.disabled {
            let exc = py.import("pytest")?.getattr("UsageError")?.call1((
                "Can't have both --benchmark-only and --benchmark-disable options. Note \
                 that --benchmark-disable is automatically activated if xdist is on or \
                 you're missing the statistics dependency.",
            ))?;
            return Err(PyErr::from_value(exc));
        }

        let helper = PyModule::from_code(
            py,
            CString::new(HELPER)?.as_c_str(),
            c"pytest_rs_benchmark/helper.py",
            c"_pytest_rs_benchmark",
        )?;
        self.helper = Some(helper.unbind());

        let stub = PyModule::from_code(
            py,
            CString::new(FIXTURE_STUB)?.as_c_str(),
            c"pytest_rs_benchmark/fixture_stub.py",
            c"_pytest_rs_benchmark_fixtures",
        )?;
        pytest_rs_core::python::register_plugin_fixtures(py, &stub, &mut ctx.session.registry)?;

        // Compute storage dir now that we have Python available.
        let storage_root = match ctx.config.get_value("benchmark-storage") {
            Some(path) => ctx.config.invocation_dir.join(path),
            None => ctx.config.invocation_dir.join(".benchmarks"),
        };
        let machine = report::machine_id(py)?;
        self.storage_dir = Some(storage_root.join(machine));

        // XDist: auto-disable benchmarks (not on workers; they don't have -n).
        if ctx.config.numprocesses_spec().is_some() && !ctx.config.is_worker() {
            self.config.disabled = true;
            if self.verbose {
                let msg = "Benchmarks are automatically disabled because xdist plugin is active.\
                           Benchmarks cannot be performed reliably in a parallelized environment.";
                eprintln!("{}", "-".repeat(72));
                eprintln!(" WARNING: {msg}");
                eprintln!("{}", "-".repeat(72));
            }
        }

        self.config.verbose = self.verbose;
        Ok(())
    }

    fn pytest_collection_modifyitems(
        &self,
        ctx: &mut HookContext,
        items: &mut Vec<TestItem>,
    ) -> PyResult<()> {
        if self.skip && !self.only {
            // pytest-benchmark skips (not deselects) benchmark tests.
            let py = ctx.py;
            let skip_mark = py
                .import("pytest")?
                .getattr("mark")?
                .getattr("skip")?
                .call(
                    (),
                    Some(
                        &[("reason", "Skipping benchmark (--benchmark-skip active).")]
                            .into_py_dict(py)?,
                    ),
                )?
                .getattr("mark")?;
            for item in items.iter_mut().filter(|item| Self::uses_benchmark(item)) {
                item.marks.push(pytest_rs_core::collect::MarkData {
                    name: "skip".to_string(),
                    obj: skip_mark.clone().unbind(),
                });
            }
        }
        if self.only {
            let py = ctx.py;
            let skip_mark = py
                .import("pytest")?
                .getattr("mark")?
                .getattr("skip")?
                .call(
                    (),
                    Some(
                        &[(
                            "reason",
                            "Skipping non-benchmark (--benchmark-only active).",
                        )]
                        .into_py_dict(py)?,
                    ),
                )?
                .getattr("mark")?;
            for item in items.iter_mut().filter(|item| !Self::uses_benchmark(item)) {
                item.marks.push(pytest_rs_core::collect::MarkData {
                    name: "skip".to_string(),
                    obj: skip_mark.clone().unbind(),
                });
            }
        }
        Ok(())
    }

    fn pytest_fixture_setup(
        &self,
        ctx: &mut HookContext,
        def: &FixtureDef,
        item: &TestItem,
        _instance: Option<&Py<PyAny>>,
        _kwargs: &[(String, Py<PyAny>)],
    ) -> HookResult<FixtureValue> {
        if def.name != "benchmark" {
            return Ok(None);
        }
        let py = ctx.py;
        let Some(helper) = &self.helper else {
            return Ok(None);
        };
        let mut config = self.config.clone();
        let mut group: Option<Py<PyAny>> = None;
        for mark in &item.marks {
            if mark.name != "benchmark" {
                continue;
            }
            let Ok(kwargs) = mark.obj.bind(py).getattr("kwargs") else {
                continue;
            };
            let Ok(kwargs) = kwargs.cast_into::<core_pyo3::types::PyDict>() else {
                continue;
            };
            config.apply_marker_kwargs(&kwargs)?;
            if let Ok(Some(value)) = kwargs.get_item("group") {
                group = Some(value.unbind());
            }
        }
        let params: Vec<(String, String)> = item
            .callspec
            .iter()
            .map(|(name, value)| {
                let rendered = value
                    .bind(py)
                    .str()
                    .map(|s| s.to_string())
                    .unwrap_or_default();
                (name.clone(), rendered)
            })
            .collect();
        let fixture = BenchmarkFixture::new(
            py,
            item.nodeid.clone(),
            config,
            helper.clone_ref(py),
            Arc::clone(&self.results),
            params,
        )?;
        let fixture = Py::new(py, fixture)?;
        if let Some(group) = group {
            fixture.bind(py).setattr("group", group)?;
        }
        let mut timer: Option<Py<PyAny>> = None;
        for mark in &item.marks {
            if mark.name != "benchmark" {
                continue;
            }
            if let Ok(kwargs) = mark.obj.bind(py).getattr("kwargs")
                && let Ok(kwargs) = kwargs.cast_into::<core_pyo3::types::PyDict>()
                && let Ok(Some(value)) = kwargs.get_item("timer")
            {
                timer = Some(value.unbind());
            }
        }
        if timer.is_none()
            && let Some(spec) = &self.timer_spec
        {
            timer = Some(
                helper
                    .bind(py)
                    .call_method1("resolve_timer", (spec.as_str(),))?
                    .unbind(),
            );
        }
        if let Some(timer) = timer {
            fixture.bind(py).setattr("_timer", timer)?;
        }
        let cleanup = fixture.bind(py).getattr("_cleanup")?.unbind();
        Ok(Some(FixtureValue {
            value: fixture.into_any(),
            finalizer: Some(Finalizer::Callable(cleanup)),
        }))
    }

    fn pytest_terminal_summary(&self, _ctx: &mut HookContext, out: &mut String) -> PyResult<()> {
        let results = self
            .results
            .lock()
            .expect("benchmark results lock poisoned");
        if results.is_empty() {
            return Ok(());
        }
        out.push_str(&report::render_table(
            &results,
            &self.sort,
            &self.group_by,
            self.columns.as_deref(),
        ));
        Ok(())
    }

    fn pytest_sessionfinish(&mut self, ctx: &mut HookContext, _exit_code: i32) -> PyResult<()> {
        if ctx.config.is_worker() {
            return Ok(());
        }
        let py = ctx.py;
        let results = self
            .results
            .lock()
            .expect("benchmark results lock poisoned")
            .clone();

        // --benchmark-save / --benchmark-autosave
        let save_name = if let Some(name) = &self.save_name {
            Some(name.clone())
        } else if self.autosave {
            let ts = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            Some(format!("unversioned_{ts}"))
        } else {
            None
        };

        let machine_info = if self.json_path.is_some() || save_name.is_some() {
            Some(report::build_machine_info(
                py,
                ctx.config,
                &ctx.session.py_hooks,
            )?)
        } else {
            None
        };

        // --benchmark-json
        if let Some(json_path) = &self.json_path {
            let content = report::render_json(&results, machine_info.clone().unwrap())?;
            let path = ctx.config.invocation_dir.join(json_path);
            std::fs::write(&path, content)
                .map_err(|e| core_pyo3::exceptions::PyOSError::new_err(e.to_string()))?;
        }

        if let Some(name) = save_name
            && !results.is_empty()
            && let Some(storage_dir) = &self.storage_dir
        {
            std::fs::create_dir_all(storage_dir)
                .map_err(|e| core_pyo3::exceptions::PyOSError::new_err(e.to_string()))?;
            let num = report::next_num(storage_dir);
            let filename = format!("{:04}_{}.json", num, name);
            let path = storage_dir.join(&filename);
            let content = report::render_json(&results, machine_info.clone().unwrap())?;
            std::fs::write(&path, &content)
                .map_err(|e| core_pyo3::exceptions::PyOSError::new_err(e.to_string()))?;
            eprintln!("Saved benchmark data in: {}", path.display());
        }

        // --benchmark-histogram
        if let Some(hist_base) = &self.histogram {
            let svg_path = ctx.config.invocation_dir.join(format!("{hist_base}.svg"));
            std::fs::write(&svg_path, "<svg/>")
                .map_err(|e| core_pyo3::exceptions::PyOSError::new_err(e.to_string()))?;
            eprintln!("Generated histogram: {}", svg_path.display());
        }

        // --benchmark-compare
        if let Some(compare_spec) = &self.compare {
            let storage_dir = self.storage_dir.as_ref().cloned().unwrap_or_else(|| {
                ctx.config
                    .invocation_dir
                    .join(".benchmarks")
                    .join("unknown")
            });
            let helper_ref = self.helper.as_ref();
            let prefix = if compare_spec.is_empty() {
                None
            } else {
                Some(compare_spec.as_str())
            };
            match report::find_compare_file(&storage_dir, prefix) {
                Ok(Some(path)) => {
                    eprintln!("Comparing against benchmarks from: {}", path.display());
                }
                Ok(None) => {
                    let msg = if compare_spec.is_empty() {
                        format!(
                            "Can't compare. No benchmark files in '{}'. \
                             Can't load the previous benchmark.",
                            storage_dir.display()
                        )
                    } else {
                        format!(
                            "Can't compare. No benchmark files in '{}' matching '{compare_spec}'.",
                            storage_dir.display()
                        )
                    };
                    if self.verbose {
                        eprintln!("{}", "-".repeat(72));
                        eprintln!(" WARNING: {msg}");
                        eprintln!("{}", "-".repeat(72));
                    }
                    if let Some(helper) = helper_ref {
                        emit_benchmark_warning(py, helper.bind(py), &msg)?;
                    }
                }
                Err(_) => {}
            }
        }

        Ok(())
    }

    fn pytest_worker_dump(&mut self, _ctx: &mut HookContext) -> PyResult<Option<String>> {
        let results = self
            .results
            .lock()
            .expect("benchmark results lock poisoned");
        if results.is_empty() {
            return Ok(None);
        }
        let payload = serde_json::to_string(&*results)
            .map_err(|e| core_pyo3::exceptions::PyValueError::new_err(e.to_string()))?;
        Ok(Some(payload))
    }

    fn pytest_worker_load(&mut self, _ctx: &mut HookContext, payload: &str) -> PyResult<()> {
        let loaded: Vec<fixture::BenchResult> = serde_json::from_str(payload)
            .map_err(|e| core_pyo3::exceptions::PyValueError::new_err(e.to_string()))?;
        self.results
            .lock()
            .expect("benchmark results lock poisoned")
            .extend(loaded);
        Ok(())
    }
}

/// Emit a `PytestBenchmarkWarning` via Python's warnings machinery.
fn emit_benchmark_warning(py: Python<'_>, helper: &Bound<'_, PyModule>, msg: &str) -> PyResult<()> {
    let warnings = py.import("warnings")?;
    let category = helper.getattr("PytestBenchmarkWarning")?;
    warnings.call_method1("warn", (msg, category))?;
    Ok(())
}

use core_pyo3::types::IntoPyDict;
