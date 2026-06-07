//! The `benchmark` fixture: a #[pyclass] backed by Rust. The timed inner
//! loop is a generated Python for-loop driven once per round (one FFI
//! crossing per round, parity with pytest-benchmark numbers).

use std::sync::{Arc, Mutex};

use pyo3::prelude::*;
use pyo3::types::{PyDict, PyModule, PyTuple};

use crate::stats::Stats;

#[derive(serde::Serialize, serde::Deserialize)]
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
}

pub type ResultStore = Arc<Mutex<Vec<BenchResult>>>;

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
    /// gc.disable() around the timed loops (--benchmark-disable-gc).
    pub disable_gc: bool,
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
            disable_gc: false,
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

#[pyclass(name = "BenchmarkFixture")]
pub struct BenchmarkFixture {
    nodeid: String,
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
    used: Mutex<Option<&'static str>>,
    #[pyo3(get)]
    extra_info: Py<PyDict>,
    #[pyo3(get, set)]
    group: Py<PyAny>,
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
        Ok(Self {
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
        })
    }

    /// Upstream's FixtureAlreadyUsed guard: the fixture runs one
    /// benchmark per test, in one mode.
    fn mark_used(&self, py: Python<'_>, mode: &'static str) -> PyResult<()> {
        let mut used = self.used.lock().expect("used lock poisoned");
        if let Some(previous) = *used {
            let exc = self
                .helper
                .bind(py)
                .getattr("FixtureAlreadyUsed")?
                .call1((format!(
                    "Fixture can only be used once. Previously it was used in {previous} mode."
                ),))?;
            return Err(PyErr::from_value(exc));
        }
        *used = Some(mode);
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
        self.results
            .lock()
            .expect("benchmark results lock poisoned")
            .push(BenchResult {
                fullname: self.nodeid.clone(),
                name,
                group,
                params: self.params.clone(),
                stats,
            });
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
            if duration >= min_time {
                return Ok((duration, loops));
            }
            if duration >= min_time_estimate {
                // Coarse estimation of the number of loops.
                loops = (min_time * loops as f64 / duration).ceil() as usize;
                if loops == 1 {
                    // Nothing to calibrate if the function is 100 times
                    // slower than the timer resolution.
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
            let times = self.run_rounds(&runner, loops, duration)?;
            self.record(py, &times, loops)?;
            // cprofile: the profiled invocations REPLACE the final plain
            // call (upstream profiles loops_range invocations).
            if self.config.cprofile {
                let result = helper.getattr("cprofile_call")?.call1((
                    func.bind(py),
                    args.bind(py),
                    &kwargs_obj,
                    loops,
                ))?;
                return Ok(result.unbind());
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

    /// Upstream's benchmark.weave (aspect mode): requires the aspectlib
    /// extra, which pytest-rs does not reproduce — same ImportError as
    /// upstream without it.
    #[pyo3(signature = (_target = None, **_kwargs))]
    fn weave(&self, _target: Option<Py<PyAny>>, _kwargs: Option<Py<PyDict>>) -> PyResult<()> {
        Err(pyo3::exceptions::PyImportError::new_err(
            "Please install aspectlib or pytest-benchmark[aspect]",
        ))
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

    #[pyo3(signature = (target, args = None, kwargs = None, setup = None, rounds = None, iterations = None, warmup_rounds = None))]
    #[allow(clippy::too_many_arguments)]
    fn pedantic(
        &self,
        py: Python<'_>,
        target: Py<PyAny>,
        args: Option<Py<PyAny>>,
        kwargs: Option<Py<PyAny>>,
        setup: Option<Py<PyAny>>,
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
        }
        if !self.config.disabled {
            self.record(py, &times, iterations)?;
        }
        // cprofile: one more invocation under the profiler (with a fresh
        // setup), like upstream's pedantic path.
        if self.config.cprofile {
            apply_setup(py, &mut call_args, &mut call_kwargs)?;
            helper.getattr("cprofile_call")?.call1((
                target.bind(py),
                call_args.bind(py),
                call_kwargs.bind(py),
            ))?;
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
