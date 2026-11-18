//! The `benchmark` fixture: a #[pyclass] backed by Rust. The timed inner
//! loop is a generated Python for-loop driven once per round (one FFI
//! crossing per round, parity with pytest-benchmark numbers).

use std::sync::{Arc, Mutex};

use pyo3::prelude::*;
use pyo3::types::{PyDict, PyModule, PyTuple};

use crate::stats::Stats;

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct BenchResult {
    pub fullname: String,
    pub name: String,
    /// The benchmark group (marker group= / runtime benchmark.group).
    #[serde(default)]
    pub group: Option<String>,
    /// @pytest.mark.parametrize params as (argname, str(value)), for
    /// --benchmark-group-by param:NAME.
    #[serde(default)]
    pub params: Vec<(String, String)>,
    pub stats: Stats,
    /// `benchmark.extra_info[...] = ...`, JSON-serialized (upstream always
    /// emits this key, `{}` when unused).
    #[serde(default)]
    pub extra_info: serde_json::Value,
    /// Per-function profile rows (--benchmark-cprofile / marker), already
    /// sorted by the configured column and capped to the top 25 like
    /// upstream's default; `None` when cprofile wasn't enabled for this run.
    #[serde(default)]
    pub cprofile: Option<Vec<serde_json::Value>>,
}

/// `json.dumps` the object then reparse as `serde_json::Value` — `extra_info`
/// only needs to be JSON-serializable (upstream's own contract), so this
/// covers arbitrary nesting without a hand-rolled PyAny→Value walk.
pub(crate) fn py_to_json(py: Python<'_>, obj: &Bound<'_, PyAny>) -> PyResult<serde_json::Value> {
    let dumped: String = py
        .import("json")?
        .call_method1("dumps", (obj,))?
        .extract()?;
    serde_json::from_str(&dumped)
        .map_err(|e| pyo3::exceptions::PyValueError::new_err(e.to_string()))
}

pub type ResultStore = Arc<Mutex<Vec<BenchResult>>>;

/// Upstream's --benchmark-cprofile-top default (rows kept per benchmark).
const CPROFILE_TOP: usize = 25;

/// Sort profile rows descending by `column` (one of the seven
/// --benchmark-cprofile choices), matching upstream's per-column type
/// (numeric for the time/count columns, string for the two name columns).
fn sort_cprofile_functions(functions: &mut [serde_json::Value], column: &str) {
    functions.sort_by(|a, b| {
        let ordering = match column {
            "ncalls_recursion" | "function_name" => a
                .get(column)
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .cmp(b.get(column).and_then(|v| v.as_str()).unwrap_or("")),
            _ => a
                .get(column)
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0)
                .partial_cmp(&b.get(column).and_then(|v| v.as_f64()).unwrap_or(0.0))
                .unwrap_or(std::cmp::Ordering::Equal),
        };
        ordering.reverse()
    });
}

#[derive(Clone)]
pub struct BenchConfig {
    pub disabled: bool,
    pub min_time: f64,
    pub max_time: f64,
    pub min_rounds: usize,
    pub calibration_precision: usize,
    pub warmup: bool,
    pub warmup_iterations: usize,
    /// One extra invocation under cProfile after the timed rounds
    /// (--benchmark-cprofile / @pytest.mark.benchmark(cprofile=True)).
    pub cprofile: bool,
    /// --benchmark-cprofile's COLUMN value (the report's sort column);
    /// defaults to "cumtime" like upstream when cprofile is marker-enabled
    /// without the CLI flag.
    pub cprofile_sort: String,
    /// gc.disable() around the timed loops (--benchmark-disable-gc).
    pub disable_gc: bool,
    /// --benchmark-verbose: print calibration progress to stderr.
    pub verbose: bool,
    /// --benchmark-precision: stop the rounds once the mean's relative
    /// margin of error falls below this fraction (5.3.0+). None = fixed
    /// number of rounds.
    pub precision: Option<f64>,
    /// --benchmark-confidence: the confidence level of that margin.
    pub confidence: Option<f64>,
}

impl Default for BenchConfig {
    fn default() -> Self {
        Self {
            disabled: false,
            min_time: 0.000005,
            max_time: 1.0,
            min_rounds: 5,
            calibration_precision: 10,
            warmup: false,
            warmup_iterations: 100_000,
            cprofile: false,
            cprofile_sort: "cumtime".to_string(),
            disable_gc: false,
            verbose: false,
            precision: None,
            confidence: Some(0.99),
        }
    }
}

impl BenchConfig {
    /// Apply @pytest.mark.benchmark(...) kwargs over the CLI/ini config,
    /// like upstream's marker-beats-options resolution.
    pub fn apply_marker_kwargs(&mut self, kwargs: &Bound<'_, PyDict>) -> PyResult<()> {
        for (key, value) in kwargs.iter() {
            let key: String = match key.extract() {
                Ok(key) => key,
                Err(_) => continue,
            };
            match key.as_str() {
                "min_time" => self.min_time = value.extract()?,
                "max_time" => self.max_time = value.extract()?,
                "min_rounds" => self.min_rounds = value.extract()?,
                "calibration_precision" => self.calibration_precision = value.extract()?,
                "warmup" => self.warmup = value.extract()?,
                "warmup_iterations" => self.warmup_iterations = value.extract()?,
                "cprofile" => self.cprofile = value.extract()?,
                "disable_gc" => self.disable_gc = value.extract()?,
                "precision" => self.precision = value.extract()?,
                "confidence" => self.confidence = value.extract()?,
                "timer" | "group" => {
                    // Handled by the caller: group lands on the fixture,
                    // timer is a callable injected into it.
                }
                _ => {}
            }
        }
        Ok(())
    }
}

/// The recorded stats exposed as `benchmark.stats` (its `.stats` property
/// returns itself, so upstream's `benchmark.stats.stats.min` works).
#[pyclass(name = "Stats")]
pub struct PyStats {
    #[pyo3(get)]
    pub min: f64,
    #[pyo3(get)]
    pub max: f64,
    #[pyo3(get)]
    pub mean: f64,
    #[pyo3(get)]
    pub stddev: f64,
    #[pyo3(get)]
    pub median: f64,
    #[pyo3(get)]
    pub iqr: f64,
    #[pyo3(get)]
    pub q1: f64,
    #[pyo3(get)]
    pub q3: f64,
    #[pyo3(get)]
    pub ops: f64,
    #[pyo3(get)]
    pub rounds: usize,
    #[pyo3(get)]
    pub iterations: usize,
    #[pyo3(get)]
    pub total: f64,
}

#[pymethods]
impl PyStats {
    #[getter]
    fn stats(slf: PyRef<'_, Self>) -> PyRef<'_, Self> {
        slf
    }
}

// module="pytest_benchmark.fixture" matches upstream's real class location —
// a bare pyclass otherwise reports __module__="builtins", which breaks tests
// asserting `repr(benchmark)` (e.g. "<pytest_benchmark.fixture.BenchmarkFixture object at ...>").
#[pyclass(name = "BenchmarkFixture", module = "pytest_benchmark.fixture")]
pub struct BenchmarkFixture {
    nodeid: String,
    /// The test's name: the last node id part, with any parameter ids.
    name: String,
    config: BenchConfig,
    helper: Py<PyModule>,
    results: ResultStore,
    recorded: Mutex<Option<Py<PyStats>>>,
    /// Injected timer (upstream's `benchmark._timer`, settable by tests);
    /// None means perf_counter.
    timer: Mutex<Option<Py<PyAny>>>,
    /// `benchmark._min_time` override (calibration tests set it directly).
    min_time_override: Mutex<Option<f64>>,
    /// @pytest.mark.parametrize params as (argname, str(value)).
    params: Vec<(String, String)>,
    /// Which mode already consumed the fixture ("benchmark(...)" or
    /// "benchmark.pedantic(...)"); a second use raises FixtureAlreadyUsed.
    used: Mutex<Option<String>>,
    #[pyo3(get)]
    extra_info: Py<PyDict>,
    #[pyo3(get, set)]
    group: Py<PyAny>,
    /// Upstream's `benchmark._cleanup_callbacks`: aspectlib rollbacks
    /// registered by `weave`/`patch`, run LIFO at fixture teardown.
    cleanup_callbacks: Mutex<Vec<Py<PyAny>>>,
}

impl BenchmarkFixture {
    pub fn new(
        py: Python<'_>,
        nodeid: String,
        config: BenchConfig,
        helper: Py<PyModule>,
        results: ResultStore,
        params: Vec<(String, String)>,
    ) -> PyResult<Self> {
        let name = nodeid.rsplit("::").next().unwrap_or(&nodeid).to_string();
        Ok(Self {
            name,
            nodeid,
            config,
            helper,
            results,
            recorded: Mutex::new(None),
            timer: Mutex::new(None),
            min_time_override: Mutex::new(None),
            params,
            used: Mutex::new(None),
            extra_info: PyDict::new(py).unbind(),
            group: py.None(),
            cleanup_callbacks: Mutex::new(Vec::new()),
        })
    }

    /// Upstream's FixtureAlreadyUsed guard: the fixture runs one
    /// benchmark per test, in one mode.
    fn mark_used(&self, py: Python<'_>, mode: &'static str) -> PyResult<()> {
        let mut used = self.used.lock().expect("used lock poisoned");
        if let Some(previous) = used.as_deref() {
            let exc = self
                .helper
                .bind(py)
                .getattr("FixtureAlreadyUsed")?
                .call1((format!(
                    "Fixture can only be used once. Previously it was used in {previous} mode."
                ),))?;
            return Err(PyErr::from_value(exc));
        }
        *used = Some(mode.to_string());
        Ok(())
    }

    /// The fraction and confidence of the precision stopping rule, when it
    /// is on (upstream: `if precision and confidence`).
    fn precision_target(&self) -> Option<(f64, f64)> {
        match (self.config.precision, self.config.confidence) {
            (Some(precision), Some(confidence)) if precision != 0.0 && confidence != 0.0 => {
                Some((precision, confidence))
            }
            _ => None,
        }
    }

    /// Upstream's `_run_until_precise`: take samples (each one round)
    /// until the relative margin of error of the mean, at `confidence`,
    /// is at most `precision` — no earlier than min_rounds (and two
    /// samples), no later than `max_rounds`. Welford's online variance.
    fn run_until_precise(
        &self,
        py: Python<'_>,
        precision: f64,
        confidence: f64,
        max_rounds: usize,
        mut sample: impl FnMut() -> PyResult<f64>,
    ) -> PyResult<()> {
        // Two-sided normal quantile, from the stdlib like upstream.
        let z: f64 = py
            .import("statistics")?
            .getattr("NormalDist")?
            .call0()?
            .call_method1("inv_cdf", (0.5 + confidence / 2.0,))?
            .extract()?;
        let mut mean = 0.0_f64;
        let mut m2 = 0.0_f64;
        for n in 1..=max_rounds {
            let duration = sample()?;
            let delta = duration - mean;
            mean += delta / n as f64;
            m2 += delta * (duration - mean);
            if n >= self.config.min_rounds && n >= 2 && mean > 0.0 {
                let stddev = (m2 / (n - 1) as f64).sqrt();
                let rel_margin = z * stddev / (n as f64).sqrt() / mean;
                if rel_margin <= precision {
                    if self.config.verbose {
                        let msg = format!(
                            "  Reached precision \u{b1}{:.2}% (target \u{b1}{:.2}%) after {n} rounds.",
                            rel_margin * 100.0,
                            precision * 100.0,
                        );
                        let _ = self
                            .helper
                            .bind(py)
                            .call_method1("_stderr_writeorg", (msg,));
                    }
                    return Ok(());
                }
            }
        }
        Ok(())
    }

    /// The injected timer or None (helper functions default to
    /// perf_counter when given None).
    fn timer_obj(&self, py: Python<'_>) -> Py<PyAny> {
        self.timer
            .lock()
            .expect("timer lock poisoned")
            .as_ref()
            .map(|t| t.clone_ref(py))
            .unwrap_or_else(|| py.None())
    }

    fn effective_min_time(&self) -> f64 {
        self.min_time_override
            .lock()
            .expect("min_time lock poisoned")
            .unwrap_or(self.config.min_time)
    }

    fn weave_impl(
        slf: PyRef<'_, Self>,
        target: Py<PyAny>,
        kwargs: Option<Py<PyDict>>,
    ) -> PyResult<()> {
        let py = slf.py();
        let kwargs_obj = match &kwargs {
            Some(k) => k.bind(py).clone(),
            None => PyDict::new(py),
        };
        let rollback = slf
            .helper
            .bind(py)
            .call_method1("weave", (&slf, target.bind(py), kwargs_obj))?;
        slf.cleanup_callbacks
            .lock()
            .expect("cleanup lock poisoned")
            .push(rollback.unbind());
        Ok(())
    }

    fn record(&self, py: Python<'_>, times: &[f64], iterations: usize) -> PyResult<()> {
        let name = self
            .nodeid
            .rsplit("::")
            .next()
            .unwrap_or(&self.nodeid)
            .to_string();
        // The group is read at record time: tests may assign
        // benchmark.group at runtime, on top of the marker's group=.
        let group = {
            let bound = self.group.bind(py);
            if bound.is_none() {
                None
            } else {
                Some(bound.str()?.to_string())
            }
        };
        let stats = Stats::from_rounds(times, iterations);
        let py_stats = Py::new(
            py,
            PyStats {
                min: stats.min,
                max: stats.max,
                mean: stats.mean,
                stddev: stats.stddev,
                median: stats.median,
                iqr: stats.iqr,
                q1: stats.q1,
                q3: stats.q3,
                ops: stats.ops,
                rounds: stats.rounds,
                iterations: stats.iterations,
                total: stats.total,
            },
        )?;
        *self.recorded.lock().expect("stats lock poisoned") = Some(py_stats);
        let extra_info = py_to_json(py, self.extra_info.bind(py))?;
        self.results
            .lock()
            .expect("benchmark results lock poisoned")
            .push(BenchResult {
                fullname: self.nodeid.clone(),
                name,
                group,
                params: self.params.clone(),
                stats,
                extra_info,
                cprofile: None,
            });
        Ok(())
    }

    /// Attach this run's --benchmark-cprofile rows (sorted by the
    /// configured column, capped to the top `CPROFILE_TOP`) to the
    /// `BenchResult` just pushed by `record`.
    fn attach_cprofile(&self, py: Python<'_>, functions: &Bound<'_, PyAny>) -> PyResult<()> {
        let json = py_to_json(py, functions)?;
        let mut list = match json {
            serde_json::Value::Array(items) => items,
            _ => Vec::new(),
        };
        sort_cprofile_functions(&mut list, &self.config.cprofile_sort);
        list.truncate(CPROFILE_TOP);
        if let Some(last) = self
            .results
            .lock()
            .expect("benchmark results lock poisoned")
            .last_mut()
        {
            last.cprofile = Some(list);
        }
        Ok(())
    }

    /// pytest-benchmark's _calibrate_timer: grow `loops` ×10 until the
    /// round duration reaches an estimation threshold, then jump straight
    /// to the projected count (bailing at 1 when the function is much
    /// slower than the timer resolution). The warmup budget re-measures
    /// inside the loop, keeping the minimum (real wall-clock bounded).
    fn calibrate(&self, py: Python<'_>, runner: &Bound<'_, PyAny>) -> PyResult<(f64, usize)> {
        let helper = self.helper.bind(py);
        let timer = self.timer_obj(py);
        let precision: f64 = helper
            .getattr("resolution")?
            .call1((timer.bind(py),))?
            .extract()?;
        let min_time = self
            .effective_min_time()
            .max(precision * self.config.calibration_precision as f64);
        let min_time_estimate = min_time * 5.0 / self.config.calibration_precision as f64;

        if self.config.verbose {
            let timer_name = timer
                .bind(py)
                .getattr("__qualname__")
                .ok()
                .and_then(|n| n.str().ok().map(|s| s.to_string()))
                .unwrap_or_else(|| "perf_counter".to_string());
            let msg = format!(
                "  Calibrating to target round {:.10}s; will estimate when reaching {:.10}s \
                 (using: {}, precision: {}).",
                min_time, min_time_estimate, timer_name, self.config.calibration_precision,
            );
            let _ = helper.call_method1("_stderr_writeorg", (msg,));
        }

        let wall_clock = helper.getattr("wall_clock")?;
        let mut loops = 1usize;
        loop {
            let mut duration: f64 = runner.call1((loops,))?.extract()?;
            if self.config.warmup {
                let warmup_start: f64 = wall_clock.call0()?.extract()?;
                let mut warmup_iterations = 0usize;
                loop {
                    let now: f64 = wall_clock.call0()?.extract()?;
                    if now - warmup_start >= self.config.max_time
                        || warmup_iterations >= self.config.warmup_iterations
                    {
                        break;
                    }
                    let measured: f64 = runner.call1((loops,))?.extract()?;
                    duration = duration.min(measured);
                    warmup_iterations += loops;
                }
            }
            if self.config.verbose {
                let msg = format!("    Measured {} iterations: {:.10}s.", loops, duration);
                let _ = helper.call_method1("_stderr_writeorg", (msg,));
            }
            if duration >= min_time {
                return Ok((duration, loops));
            }
            if duration >= min_time_estimate {
                loops = (min_time * loops as f64 / duration).ceil() as usize;
                if loops == 1 {
                    return Ok((duration, loops));
                }
            } else {
                loops = loops.saturating_mul(10);
            }
        }
    }

    fn run_rounds(
        &self,
        runner: &Bound<'_, PyAny>,
        loops: usize,
        round_duration: f64,
    ) -> PyResult<Vec<f64>> {
        let rounds = self.round_count(round_duration);
        let mut times = Vec::with_capacity(rounds);
        for _ in 0..rounds {
            times.push(runner.call1((loops,))?.extract()?);
        }
        Ok(times)
    }

    /// ceil(max_time / duration) clamped below by min_rounds (upstream).
    fn round_count(&self, round_duration: f64) -> usize {
        ((self.config.max_time / round_duration.max(1e-12)).ceil() as usize)
            .max(self.config.min_rounds)
            .min(100_000_000)
    }
}

#[pymethods]
impl BenchmarkFixture {
    #[pyo3(signature = (func, *args, **kwargs))]
    fn __call__(
        &self,
        py: Python<'_>,
        func: Py<PyAny>,
        args: Py<PyTuple>,
        kwargs: Option<Py<PyDict>>,
    ) -> PyResult<Py<PyAny>> {
        self.mark_used(py, "benchmark(...)")?;
        let helper = self.helper.bind(py);
        let kwargs_obj = match &kwargs {
            Some(kwargs) => kwargs.bind(py).as_any().clone(),
            None => PyDict::new(py).into_any(),
        };
        if !self.config.disabled {
            let timer = self.timer_obj(py);
            let runner = helper.getattr("make_runner")?.call1((
                func.bind(py),
                args.bind(py),
                &kwargs_obj,
                timer.bind(py),
                self.config.disable_gc,
            ))?;
            let (duration, loops) = self.calibrate(py, &runner)?;
            let rounds = self.round_count(duration);
            // Pre-measurement warmup rounds, bounded like upstream's _raw.
            if self.config.warmup {
                let warmup_rounds =
                    rounds.min((self.config.warmup_iterations / loops.max(1)).max(1));
                for _ in 0..warmup_rounds {
                    runner.call1((loops,))?;
                }
            }
            if self.config.verbose {
                let msg = format!("  Running {} rounds x {} iterations ...", rounds, loops);
                let _ = helper.call_method1("_stderr_writeorg", (msg,));
            }
            let times = match self.precision_target() {
                None => self.run_rounds(&runner, loops, duration)?,
                Some((precision, confidence)) => {
                    let mut times = Vec::new();
                    self.run_until_precise(py, precision, confidence, rounds, || {
                        let measured: f64 = runner.call1((loops,))?.extract()?;
                        times.push(measured);
                        Ok(measured)
                    })?;
                    times
                }
            };
            if self.config.verbose {
                let total: f64 = times.iter().sum();
                let msg = format!("  Ran for {:.10}s.", total);
                let _ = helper.call_method1("_stderr_writeorg", (msg,));
            }
            self.record(py, &times, loops)?;
            // cprofile: the profiled invocations REPLACE the final plain
            // call (upstream profiles loops_range invocations).
            if self.config.cprofile {
                let (result, functions): (Py<PyAny>, Py<PyAny>) = helper
                    .getattr("cprofile_call")?
                    .call1((func.bind(py), args.bind(py), &kwargs_obj, loops))?
                    .extract()?;
                self.attach_cprofile(py, functions.bind(py))?;
                return Ok(result);
            }
        }
        // The caller's return value comes from one plain final call, like
        // upstream's _raw (also the only call in disabled mode).
        let kwargs_dict = kwargs_obj.cast::<PyDict>()?;
        let result = func
            .bind(py)
            .call(args.bind(py).clone(), Some(kwargs_dict))?;
        Ok(result.unbind())
    }

    /// Upstream's `benchmark._timer` (calibration tests inject fake
    /// timers directly on the fixture).
    #[getter(_timer)]
    fn get_timer(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        if let Some(timer) = self.timer.lock().expect("timer lock poisoned").as_ref() {
            return Ok(timer.clone_ref(py));
        }
        Ok(py.import("time")?.getattr("perf_counter")?.unbind())
    }

    #[setter(_timer)]
    fn set_timer(&self, value: Py<PyAny>) {
        *self.timer.lock().expect("timer lock poisoned") = Some(value);
    }

    /// Upstream's `benchmark._min_time` (settable for calibration tests).
    #[getter(_min_time)]
    fn get_min_time(&self) -> f64 {
        self.effective_min_time()
    }

    #[setter(_min_time)]
    fn set_min_time(&self, value: f64) {
        *self
            .min_time_override
            .lock()
            .expect("min_time lock poisoned") = Some(value);
    }

    /// Upstream's constructor, for code that builds a fixture by hand (its
    /// own tests do, with a stand-in node). The engine creates the fixture
    /// it hands to tests through `BenchmarkFixture::new` instead;
    /// `add_stats`, `logger` and `warner` are accepted for signature
    /// compatibility, results go to a store of this fixture's own.
    #[new]
    #[pyo3(signature = (node, disable_gc, timer, min_rounds, min_time, max_time, precision, confidence, warmup, warmup_iterations, calibration_precision, add_stats, logger, warner, disabled, cprofile, cprofile_loops, cprofile_dump, group = None))]
    #[allow(clippy::too_many_arguments)]
    fn py_new(
        py: Python<'_>,
        node: &Bound<'_, PyAny>,
        disable_gc: bool,
        timer: &Bound<'_, PyAny>,
        min_rounds: usize,
        min_time: f64,
        max_time: f64,
        precision: Option<f64>,
        confidence: Option<f64>,
        warmup: bool,
        warmup_iterations: usize,
        calibration_precision: usize,
        add_stats: &Bound<'_, PyAny>,
        logger: &Bound<'_, PyAny>,
        warner: &Bound<'_, PyAny>,
        disabled: bool,
        cprofile: Option<&Bound<'_, PyAny>>,
        cprofile_loops: Option<&Bound<'_, PyAny>>,
        cprofile_dump: &Bound<'_, PyAny>,
        group: Option<Py<PyAny>>,
    ) -> PyResult<Self> {
        let _ = (add_stats, logger, warner, cprofile_loops, cprofile_dump);
        let precise = precision
            .zip(confidence)
            .filter(|(p, c)| *p != 0.0 && *c != 0.0);
        let config = BenchConfig {
            disabled,
            min_time,
            max_time,
            min_rounds,
            calibration_precision,
            warmup,
            warmup_iterations,
            cprofile: cprofile.is_some_and(|value| value.is_truthy().unwrap_or(false)),
            disable_gc,
            precision: precise.map(|(p, _)| p),
            confidence: precise.map(|(_, c)| c),
            ..BenchConfig::default()
        };
        let helper = PyModule::from_code(
            py,
            std::ffi::CString::new(crate::HELPER)?.as_c_str(),
            c"pytest_rs_benchmark/helper.py",
            c"_pytest_rs_benchmark",
        )?;
        let mut fixture = Self::new(
            py,
            node.getattr("nodeid")?.extract()?,
            config,
            helper.unbind(),
            Arc::new(Mutex::new(Vec::new())),
            Vec::new(),
        )?;
        fixture.name = node.getattr("name")?.extract()?;
        // Upstream stores `timer.target` (a NameWrapper) as `_timer`.
        let target = timer.getattr("target").unwrap_or_else(|_| timer.clone());
        fixture.timer = Mutex::new(Some(target.unbind()));
        if let Some(group) = group {
            fixture.group = group;
        }
        Ok(fixture)
    }

    /// The test's name (upstream: `node.name`).
    #[getter]
    fn name(&self) -> String {
        self.name.clone()
    }

    /// The test's node id (upstream: `node.nodeid`).
    #[getter]
    fn fullname(&self) -> String {
        self.nodeid.clone()
    }

    /// Upstream's `benchmark._mode`: which call consumed the fixture
    /// (None while unused). Tests that drive internals directly set it to
    /// mark the fixture as used.
    #[getter(_mode)]
    fn get_mode(&self) -> Option<String> {
        self.used.lock().expect("used lock poisoned").clone()
    }

    #[setter(_mode)]
    fn set_mode(&self, value: Option<String>) {
        *self.used.lock().expect("used lock poisoned") = value;
    }

    #[getter(_min_rounds)]
    fn get_min_rounds(&self) -> usize {
        self.config.min_rounds
    }

    #[setter(_min_rounds)]
    fn set_min_rounds(&mut self, value: usize) {
        self.config.min_rounds = value;
    }

    #[getter(_precision)]
    fn get_precision(&self) -> Option<f64> {
        self.config.precision
    }

    #[setter(_precision)]
    fn set_precision(&mut self, value: Option<f64>) {
        self.config.precision = value;
    }

    #[getter(_confidence)]
    fn get_confidence(&self) -> Option<f64> {
        self.config.confidence
    }

    #[setter(_confidence)]
    fn set_confidence(&mut self, value: Option<f64>) {
        self.config.confidence = value;
    }

    /// Upstream's `_run_until_precise(runner, loops_range, stats,
    /// max_rounds)`: each round is `runner(loops_range)`, fed to
    /// `stats.update(...)`.
    fn _run_until_precise(
        &self,
        py: Python<'_>,
        runner: &Bound<'_, PyAny>,
        loops_range: &Bound<'_, PyAny>,
        stats: &Bound<'_, PyAny>,
        max_rounds: usize,
    ) -> PyResult<()> {
        let precision = self.config.precision.unwrap_or(0.0);
        let confidence = self.config.confidence.unwrap_or(0.0);
        self.run_until_precise(py, precision, confidence, max_rounds, || {
            let duration = runner.call1((loops_range,))?;
            stats.call_method1("update", (&duration,))?;
            duration.extract()
        })
    }

    /// Upstream's benchmark.weave (aspect mode): weaves `self(target, ...)`
    /// into `target` via aspectlib (requires the optional aspectlib extra;
    /// same ImportError as upstream without it). The rollback is deferred
    /// to fixture teardown, mirroring upstream's `_cleanup_callbacks`.
    #[pyo3(signature = (target, **kwargs))]
    fn weave(slf: PyRef<'_, Self>, target: Py<PyAny>, kwargs: Option<Py<PyDict>>) -> PyResult<()> {
        Self::weave_impl(slf, target, kwargs)
    }

    /// `patch` is upstream's alias for `weave` (`patch = weave`).
    #[pyo3(signature = (target, **kwargs))]
    fn patch(slf: PyRef<'_, Self>, target: Py<PyAny>, kwargs: Option<Py<PyDict>>) -> PyResult<()> {
        Self::weave_impl(slf, target, kwargs)
    }

    /// Upstream's `benchmark._cleanup`: runs aspectlib rollbacks LIFO.
    /// Invoked as the fixture's teardown finalizer.
    fn _cleanup(&self, py: Python<'_>) -> PyResult<()> {
        loop {
            let callback = self
                .cleanup_callbacks
                .lock()
                .expect("cleanup lock poisoned")
                .pop();
            let Some(callback) = callback else {
                return Ok(());
            };
            callback.bind(py).call0()?;
        }
    }

    #[getter]
    fn disabled(&self) -> bool {
        self.config.disabled
    }

    #[getter]
    fn enabled(&self) -> bool {
        !self.config.disabled
    }

    #[getter]
    fn stats(&self, py: Python<'_>) -> Option<Py<PyStats>> {
        self.recorded
            .lock()
            .expect("stats lock poisoned")
            .as_ref()
            .map(|stats| stats.clone_ref(py))
    }

    #[pyo3(signature = (target, args = None, kwargs = None, setup = None, teardown = None, rounds = None, iterations = None, warmup_rounds = None))]
    #[allow(clippy::too_many_arguments)]
    fn pedantic(
        &self,
        py: Python<'_>,
        target: Py<PyAny>,
        args: Option<Py<PyAny>>,
        kwargs: Option<Py<PyAny>>,
        setup: Option<Py<PyAny>>,
        teardown: Option<Py<PyAny>>,
        rounds: Option<Py<PyAny>>,
        iterations: Option<Py<PyAny>>,
        warmup_rounds: Option<Py<PyAny>>,
    ) -> PyResult<Py<PyAny>> {
        self.mark_used(py, "benchmark.pedantic(...)")?;
        // Validation before anything runs (upstream raises without calling
        // setup or the target).
        let rounds = positive_int(py, rounds.as_ref(), 1, 1, "rounds")?;
        let iterations = positive_int(py, iterations.as_ref(), 1, 1, "iterations")?;
        let warmup_rounds = positive_int(py, warmup_rounds.as_ref(), 0, 0, "warmup_rounds")?;
        if setup.is_some() && iterations > 1 {
            return Err(pyo3::exceptions::PyValueError::new_err(
                "Can't use more than 1 `iterations` with a `setup` function.",
            ));
        }
        let explicit_args = args.is_some() || kwargs.is_some();

        let helper = self.helper.bind(py);
        let empty_tuple = PyTuple::empty(py).into_any().unbind();
        let mut call_args = args.unwrap_or(empty_tuple);
        let mut call_kwargs = match kwargs {
            Some(kwargs) => kwargs,
            None => PyDict::new(py).into_any().unbind(),
        };

        // setup runs before each round; it may return fresh (args, kwargs),
        // which conflicts with explicitly-passed ones.
        let apply_setup = |py: Python<'_>,
                           call_args: &mut Py<PyAny>,
                           call_kwargs: &mut Py<PyAny>|
         -> PyResult<()> {
            if let Some(setup) = &setup {
                let produced = setup.bind(py).call0()?;
                if !produced.is_none() {
                    if explicit_args {
                        return Err(pyo3::exceptions::PyTypeError::new_err(
                            "Can't use `args` or `kwargs` if `setup` returns the arguments.",
                        ));
                    }
                    let (new_args, new_kwargs): (Py<PyAny>, Py<PyAny>) = produced.extract()?;
                    *call_args = new_args;
                    *call_kwargs = new_kwargs;
                }
            }
            Ok(())
        };

        // teardown (5.2.0+) gets the same arguments as the target, after
        // every call of it that ran (warmup, timed, extra and profiled).
        let apply_teardown =
            |py: Python<'_>, call_args: &Py<PyAny>, call_kwargs: &Py<PyAny>| -> PyResult<()> {
                if let Some(teardown) = &teardown {
                    let args = call_args
                        .bind(py)
                        .try_iter()?
                        .collect::<PyResult<Vec<_>>>()?;
                    let args = PyTuple::new(py, args)?;
                    let kwargs = call_kwargs.bind(py).cast::<PyDict>()?;
                    teardown.bind(py).call(args, Some(kwargs))?;
                }
                Ok(())
            };

        let timer = self.timer_obj(py);
        let make_result_runner = helper.getattr("make_result_runner")?;
        for _ in 0..warmup_rounds {
            apply_setup(py, &mut call_args, &mut call_kwargs)?;
            let runner = make_result_runner.call1((
                target.bind(py),
                call_args.bind(py),
                call_kwargs.bind(py),
                timer.bind(py),
            ))?;
            runner.call1((iterations,))?;
            apply_teardown(py, &call_args, &call_kwargs)?;
        }

        let mut times = Vec::with_capacity(rounds);
        let mut result = py.None();
        for _ in 0..rounds {
            apply_setup(py, &mut call_args, &mut call_kwargs)?;
            let runner = make_result_runner.call1((
                target.bind(py),
                call_args.bind(py),
                call_kwargs.bind(py),
                timer.bind(py),
            ))?;
            let (duration, round_result): (f64, Py<PyAny>) =
                runner.call1((iterations,))?.extract()?;
            times.push(duration);
            result = round_result;
            apply_teardown(py, &call_args, &call_kwargs)?;
        }
        if iterations > 1 {
            // Upstream makes one extra plain call for the return value when
            // iterating (the timed loop discards results).
            apply_setup(py, &mut call_args, &mut call_kwargs)?;
            let (_, extra_result): (f64, Py<PyAny>) = helper
                .getattr("timed_call")?
                .call1((target.bind(py), call_args.bind(py), call_kwargs.bind(py)))?
                .extract()?;
            result = extra_result;
            apply_teardown(py, &call_args, &call_kwargs)?;
        }
        if !self.config.disabled {
            self.record(py, &times, iterations)?;
            // cprofile: one more invocation under the profiler (with a
            // fresh setup), like upstream's pedantic path. Gated on
            // `!disabled` alongside `record` above: attach_cprofile targets
            // the `BenchResult` `record` just pushed, so running it without
            // a matching `record` call would attach these rows to a
            // different (stale) benchmark's result.
            if self.config.cprofile {
                apply_setup(py, &mut call_args, &mut call_kwargs)?;
                let (_, functions): (Py<PyAny>, Py<PyAny>) = helper
                    .getattr("cprofile_call")?
                    .call1((target.bind(py), call_args.bind(py), call_kwargs.bind(py)))?
                    .extract()?;
                apply_teardown(py, &call_args, &call_kwargs)?;
                self.attach_cprofile(py, functions.bind(py))?;
            }
        }
        Ok(result)
    }
}

/// Extract a rounds/iterations-style argument: ValueError (not TypeError)
/// on non-int or below `min`, like pytest-benchmark.
fn positive_int(
    py: Python<'_>,
    value: Option<&Py<PyAny>>,
    default: i64,
    min: i64,
    what: &str,
) -> PyResult<usize> {
    let Some(value) = value else {
        return Ok(default as usize);
    };
    let value: i64 = value.bind(py).extract().map_err(|_| {
        pyo3::exceptions::PyValueError::new_err(format!("{what} must be an integer"))
    })?;
    if value < min {
        return Err(pyo3::exceptions::PyValueError::new_err(format!(
            "{what} must be at least {min}"
        )));
    }
    Ok(value as usize)
}
