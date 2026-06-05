//! pytest-split equivalent: split the collected items into N groups of
//! roughly equal cached duration and run only one of them.

mod algorithms;

use std::collections::HashMap;
use std::path::PathBuf;

use pytest_rs_core::collect::TestItem;
use pytest_rs_core::config::{OptDef, OptionParser};
use pytest_rs_core::hooks::{HookContext, Plugin};
use pytest_rs_core::pyo3::prelude::*;
use pytest_rs_core::report::Phase;

/// Setup/teardown durations above this are bogus (freezegun); ignored when
/// storing, like upstream.
const STORE_DURATIONS_SETUP_AND_TEARDOWN_THRESHOLD: f64 = 60.0 * 10.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Algorithm {
    DurationBasedChunks,
    LeastDuration,
}

pub struct SplitPlugin {
    splits: Option<usize>,
    group: Option<usize>,
    algorithm: Algorithm,
    store_durations: bool,
    clean_durations: bool,
    durations_path: PathBuf,
    cached_durations: HashMap<String, f64>,
}

impl SplitPlugin {
    pub fn new() -> Self {
        Self {
            splits: None,
            group: None,
            algorithm: Algorithm::DurationBasedChunks,
            store_durations: false,
            clean_durations: false,
            durations_path: PathBuf::new(),
            cached_durations: HashMap::new(),
        }
    }

    fn load_durations(path: &PathBuf) -> HashMap<String, f64> {
        let Ok(content) = std::fs::read_to_string(path) else {
            return HashMap::new();
        };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&content) else {
            return HashMap::new();
        };
        match value {
            serde_json::Value::Object(map) => map
                .into_iter()
                .filter_map(|(name, duration)| duration.as_f64().map(|d| (name, d)))
                .collect(),
            // Legacy list-of-pairs format.
            serde_json::Value::Array(pairs) => pairs
                .into_iter()
                .filter_map(|pair| {
                    let name = pair.get(0)?.as_str()?.to_string();
                    let duration = pair.get(1)?.as_f64()?;
                    Some((name, duration))
                })
                .collect(),
            _ => HashMap::new(),
        }
    }
}

impl Default for SplitPlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl Plugin for SplitPlugin {
    fn name(&self) -> &str {
        "split"
    }

    fn pytest_addoption(&self, parser: &mut OptionParser) {
        parser.add_option(OptDef::value(
            "--splits",
            None,
            "The number of groups to split the tests into",
        ));
        parser.add_option(OptDef::value(
            "--group",
            None,
            "The group of tests that should be executed (first one is 1)",
        ));
        parser.add_option(OptDef::value(
            "--splitting-algorithm",
            Some("duration_based_chunks"),
            "Algorithm used to split the tests. Choices: ['duration_based_chunks', 'least_duration']",
        ));
        parser.add_option(OptDef::flag(
            "--store-durations",
            "Store durations into '--durations-path'",
        ));
        parser.add_option(OptDef::value(
            "--durations-path",
            None,
            "Path to the file in which durations are (to be) stored, \
             default is .test_durations in the current working directory",
        ));
        parser.add_option(OptDef::flag(
            "--clean-durations",
            "Removes the test duration info for tests which are not present \
             while running the suite with '--store-durations'",
        ));
    }

    fn pytest_configure(&mut self, ctx: &mut HookContext) -> PyResult<()> {
        let py = ctx.py;

        // Choice validation first (argparse rejects at parse time upstream).
        self.algorithm = match ctx
            .config
            .get_value("splitting-algorithm")
            .unwrap_or("duration_based_chunks")
        {
            "duration_based_chunks" => Algorithm::DurationBasedChunks,
            "least_duration" => Algorithm::LeastDuration,
            other => {
                return Err(pytest_rs_core::python::usage_error(
                    py,
                    &format!(
                        "argument --splitting-algorithm: invalid choice: '{other}' \
                         (choose from 'duration_based_chunks', 'least_duration')"
                    ),
                ));
            }
        };

        let splits = ctx.config.get_value("splits");
        let group = ctx.config.get_value("group");

        if splits.is_some() && group.is_none() {
            return Err(pytest_rs_core::python::usage_error(
                py,
                "argument `--group` is required",
            ));
        }
        if group.is_some() && splits.is_none() {
            return Err(pytest_rs_core::python::usage_error(
                py,
                "argument `--splits` is required",
            ));
        }

        if let (Some(splits), Some(group)) = (splits, group) {
            let splits: usize = splits.parse().map_err(|_| {
                pytest_rs_core::python::usage_error(py, "argument `--splits` must be an integer")
            })?;
            let group: usize = group.parse().map_err(|_| {
                pytest_rs_core::python::usage_error(py, "argument `--group` must be an integer")
            })?;
            if splits < 1 {
                return Err(pytest_rs_core::python::usage_error(
                    py,
                    "argument `--splits` must be >= 1",
                ));
            }
            if group < 1 || group > splits {
                return Err(pytest_rs_core::python::usage_error(
                    py,
                    &format!("argument `--group` must be >= 1 and <= {splits}"),
                ));
            }
            self.splits = Some(splits);
            self.group = Some(group);
        }

        self.store_durations = ctx.config.get_flag("store-durations");
        self.clean_durations = ctx.config.get_flag("clean-durations");
        self.durations_path = match ctx.config.get_value("durations-path") {
            Some(path) => PathBuf::from(path),
            None => ctx.config.rootdir.join(".test_durations"),
        };

        if self.splits.is_some() || self.store_durations {
            self.cached_durations = Self::load_durations(&self.durations_path);
        }
        if self.splits.is_some() && self.cached_durations.is_empty() && !ctx.config.no_terminal() {
            println!(
                "\n[pytest-split] No test durations found. Pytest-split will \
                 split tests evenly when no durations are found. \
                 \n[pytest-split] You can expect better results in consequent runs, \
                 when test timings have been documented.\n"
            );
        }
        Ok(())
    }

    fn pytest_collection_modifyitems(
        &self,
        ctx: &mut HookContext,
        items: &mut Vec<TestItem>,
    ) -> PyResult<()> {
        let (Some(splits), Some(group)) = (self.splits, self.group) else {
            return Ok(());
        };
        let nodeids: Vec<String> = items.iter().map(|item| item.nodeid.clone()).collect();
        let groups = match self.algorithm {
            Algorithm::DurationBasedChunks => {
                algorithms::duration_based_chunks(splits, &nodeids, &self.cached_durations)
            }
            Algorithm::LeastDuration => {
                algorithms::least_duration(splits, &nodeids, &self.cached_durations)
            }
        };
        let selected = &groups[group - 1];

        let mut keep: Vec<bool> = vec![false; items.len()];
        for &index in &selected.selected {
            keep[index] = true;
        }
        let mut index = 0usize;
        items.retain(|_| {
            let kept = keep[index];
            index += 1;
            kept
        });

        if !ctx.config.no_terminal() {
            let algorithm = match self.algorithm {
                Algorithm::DurationBasedChunks => "duration_based_chunks",
                Algorithm::LeastDuration => "least_duration",
            };
            println!("\n\n[pytest-split] Splitting tests with algorithm: {algorithm}");
            println!(
                "[pytest-split] Running group {group}/{splits} (estimated duration: {:.2}s)\n",
                selected.duration
            );
        }
        Ok(())
    }

    fn pytest_sessionfinish(&mut self, ctx: &mut HookContext, _exit_code: i32) -> PyResult<()> {
        if !self.store_durations {
            return Ok(());
        }
        // Under -n the parent holds the merged reports; workers writing the
        // durations file would race and store partial data.
        if ctx.config.is_worker() {
            return Ok(());
        }
        let mut test_durations: HashMap<String, f64> = HashMap::new();
        for report in &ctx.session.reports {
            let duration = report.duration.as_secs_f64();
            if matches!(report.phase, Phase::Setup | Phase::Teardown)
                && duration > STORE_DURATIONS_SETUP_AND_TEARDOWN_THRESHOLD
            {
                continue;
            }
            *test_durations.entry(report.nodeid.clone()).or_default() += duration;
        }

        if self.clean_durations {
            self.cached_durations = test_durations;
        } else {
            self.cached_durations.extend(test_durations);
        }

        let sorted: std::collections::BTreeMap<&String, f64> = self
            .cached_durations
            .iter()
            .map(|(name, duration)| (name, *duration))
            .collect();
        let content = serde_json::to_string_pretty(&sorted)
            .map_err(|e| pytest_rs_core::pyo3::exceptions::PyValueError::new_err(e.to_string()))?;
        std::fs::write(&self.durations_path, content)
            .map_err(|e| pytest_rs_core::pyo3::exceptions::PyOSError::new_err(e.to_string()))?;

        if !ctx.config.no_terminal() {
            println!(
                "\n\n[pytest-split] Stored test durations in {}",
                self.durations_path.display()
            );
        }
        Ok(())
    }
}
