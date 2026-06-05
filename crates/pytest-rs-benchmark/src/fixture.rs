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
        }
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
    ) -> PyResult<Self> {
        Ok(Self {
            nodeid,
            config,
            helper,
            results,
            recorded: Mutex::new(None),
            extra_info: PyDict::new(py).unbind(),
            group: py.None(),
        })
    }

    fn record(&self, py: Python<'_>, times: &[f64], iterations: usize) -> PyResult<()> {
        let name = self
            .nodeid
            .rsplit("::")
            .next()
            .unwrap_or(&self.nodeid)
            .to_string();
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
                stats,
            });
        Ok(())
    }

    /// Grow `loops` until one round meets min_time (pytest-benchmark's
    /// calibration loop).
    fn calibrate(
        &self,
        py: Python<'_>,
        runner: &Bound<'_, PyAny>,
        first_duration: f64,
    ) -> PyResult<(usize, f64)> {
        let helper = self.helper.bind(py);
        let resolution: f64 = helper.getattr("resolution")?.call0()?.extract()?;
        let min_time = self
            .config
            .min_time
            .max(resolution * self.config.calibration_precision as f64);

        let mut loops = 1usize;
        let mut duration = first_duration;
        while duration < min_time {
            loops = if duration < min_time / 10.0 {
                loops.saturating_mul(10)
            } else {
                loops.saturating_mul(2)
            };
            duration = runner.call1((loops,))?.extract()?;
        }
        Ok((loops, duration))
    }

    fn run_rounds(
        &self,
        runner: &Bound<'_, PyAny>,
        loops: usize,
        round_duration: f64,
    ) -> PyResult<Vec<f64>> {
        let rounds = ((self.config.max_time / round_duration.max(1e-9)).ceil() as usize)
            .clamp(self.config.min_rounds, 10_000);
        let mut times = Vec::with_capacity(rounds);
        for _ in 0..rounds {
            times.push(runner.call1((loops,))?.extract()?);
        }
        Ok(times)
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
        let helper = self.helper.bind(py);
        let kwargs_obj = match &kwargs {
            Some(kwargs) => kwargs.bind(py).as_any().clone(),
            None => PyDict::new(py).into_any(),
        };
        // One timed call: calibration seed plus the caller's return value.
        let (first_duration, result): (f64, Py<PyAny>) = helper
            .getattr("timed_call")?
            .call1((func.bind(py), args.bind(py), &kwargs_obj))?
            .extract()?;
        if self.config.disabled {
            return Ok(result);
        }

        let runner =
            helper
                .getattr("make_runner")?
                .call1((func.bind(py), args.bind(py), &kwargs_obj))?;
        if self.config.warmup {
            let warmup_loops = self.config.warmup_iterations.max(1);
            runner.call1((warmup_loops,))?;
        }
        let (loops, duration) = self.calibrate(py, &runner, first_duration)?;
        let times = self.run_rounds(&runner, loops, duration)?;
        self.record(py, &times, loops)?;
        Ok(result)
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

        let make_result_runner = helper.getattr("make_result_runner")?;
        for _ in 0..warmup_rounds {
            apply_setup(py, &mut call_args, &mut call_kwargs)?;
            let runner = make_result_runner.call1((
                target.bind(py),
                call_args.bind(py),
                call_kwargs.bind(py),
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
