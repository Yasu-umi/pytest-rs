//! pytest-benchmark equivalent: the `benchmark` fixture (Rust #[pyclass],
//! Python inner loop), calibration, stats, terminal table, --benchmark-json.

mod fixture;
mod report;
mod stats;

use std::ffi::CString;
use std::sync::{Arc, Mutex};

use fixture::{BenchConfig, BenchmarkFixture, ResultStore};
use pytest_rs_core::collect::TestItem;
use pytest_rs_core::config::{OptDef, OptionParser};
use pytest_rs_core::fixture::FixtureDef;
use pytest_rs_core::hooks::{FixtureValue, HookContext, HookResult, Plugin};
use pytest_rs_core::pyo3 as core_pyo3;

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
"#;

pub struct BenchmarkPlugin {
    config: BenchConfig,
    only: bool,
    skip: bool,
    sort: String,
    json_path: Option<String>,
    helper: Option<Py<PyModule>>,
    results: ResultStore,
}

impl BenchmarkPlugin {
    pub fn new() -> Self {
        Self {
            config: BenchConfig::default(),
            only: false,
            skip: false,
            sort: "min".to_string(),
            json_path: None,
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
            "Column to sort the result table by: min, max, mean, stddev, name",
        ));
        parser.add_option(OptDef::value(
            "--benchmark-cprofile",
            None,
            "Run the benchmarked function once more under cProfile (the \
             profile table column to sort by upstream; table not rendered)",
        ));
        // Accepted-but-inert pytest-benchmark options (storage/comparison
        // features are not reproduced).
        for inert in [
            "--benchmark-group-by",
            "--benchmark-timer",
            "--benchmark-save",
            "--benchmark-compare",
            "--benchmark-storage",
            "--benchmark-histogram",
            "--benchmark-columns",
            "--benchmark-name",
        ] {
            parser.add_option(OptDef::value(inert, None, "accepted but inert"));
        }
        parser.add_option(OptDef::flag("--benchmark-autosave", "accepted but inert"));
        parser.add_option(OptDef::flag("--benchmark-verbose", "accepted but inert"));
    }

    fn pytest_configure(&mut self, ctx: &mut HookContext) -> PyResult<()> {
        let py = ctx.py;
        self.only = ctx.config.get_flag("benchmark-only");
        self.skip = ctx.config.get_flag("benchmark-skip");
        self.config.disabled = ctx.config.get_flag("benchmark-disable");
        self.json_path = ctx.config.get_value("benchmark-json").map(str::to_string);
        if let Some(value) = ctx.config.get_value("benchmark-min-rounds") {
            self.config.min_rounds = value.parse().unwrap_or(self.config.min_rounds);
        }
        if let Some(value) = ctx.config.get_value("benchmark-min-time") {
            self.config.min_time = value.parse().unwrap_or(self.config.min_time);
        }
        if let Some(value) = ctx.config.get_value("benchmark-max-time") {
            self.config.max_time = value.parse().unwrap_or(self.config.max_time);
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
            self.sort = value.to_string();
        }
        if ctx.config.get_value("benchmark-cprofile").is_some() {
            self.config.cprofile = true;
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
        Ok(())
    }

    fn pytest_collection_modifyitems(
        &self,
        ctx: &mut HookContext,
        items: &mut Vec<TestItem>,
    ) -> PyResult<()> {
        if self.only {
            items.retain(Self::uses_benchmark);
        }
        if self.skip {
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
        // @pytest.mark.benchmark(...) kwargs override the CLI/ini config
        // for this item; `group` lands on the fixture itself.
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
        let fixture = BenchmarkFixture::new(
            py,
            item.nodeid.clone(),
            config,
            helper.clone_ref(py),
            Arc::clone(&self.results),
        )?;
        let fixture = Py::new(py, fixture)?;
        if let Some(group) = group {
            fixture.bind(py).setattr("group", group)?;
        }
        Ok(Some(FixtureValue {
            value: fixture.into_any(),
            finalizer: None,
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
        out.push_str(&report::render_table(&results, &self.sort));
        Ok(())
    }

    fn pytest_sessionfinish(&mut self, ctx: &mut HookContext, _exit_code: i32) -> PyResult<()> {
        if ctx.config.is_worker() {
            // Results travel to the parent (pytest_worker_dump) instead.
            return Ok(());
        }
        let Some(json_path) = &self.json_path else {
            return Ok(());
        };
        let results = self
            .results
            .lock()
            .expect("benchmark results lock poisoned");
        let content = report::render_json(ctx.py, &results)?;
        let path = ctx.config.invocation_dir.join(json_path);
        std::fs::write(&path, content)
            .map_err(|e| core_pyo3::exceptions::PyOSError::new_err(e.to_string()))?;
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

use core_pyo3::types::IntoPyDict;
