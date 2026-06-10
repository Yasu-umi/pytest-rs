//! cacheprovider parity: --lf/--ff/--nf selection plus the
//! cache/lastfailed and cache/nodeids stores.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use pyo3::prelude::*;

use crate::collect::TestItem;
use crate::config::Config;
use crate::report::{Outcome, Phase, TestReport};

pub struct CacheState {
    enabled: bool,
    lf: bool,
    ff: bool,
    nf: bool,
    /// --stepwise: resume from the cached failure, stop at the next one.
    sw: bool,
    /// True when --sw-reset was used.
    sw_reset: bool,
    /// The nodeid the previous --stepwise run stopped at, plus cache metadata.
    sw_resume: Option<(String, Option<usize>, Option<String>)>,
    /// Error message when the cache existed but was invalid (e.g. corrupted).
    sw_cache_error: Option<String>,
    /// Failed nodeids from the previous run, in recorded order.
    lastfailed: Vec<String>,
    /// Nodeids seen in previous runs (--nf sorts unseen ones first).
    cached_nodeids: Vec<String>,
    /// Files not collected because --lf restricted collection.
    skipped_files: usize,
    /// The "run-last-failure: ..." collection status line.
    report_status: Option<String>,
    /// The "stepwise: ..." collection status lines (may include error + normal).
    sw_statuses: Vec<String>,
    /// Number of items seen in the current collection (for writing cache).
    total_items: usize,
}

impl CacheState {
    pub fn new(py: Python<'_>, config: &Config) -> Self {
        let enabled = !config.plugin_disabled("cacheprovider");
        if enabled && config.get_flag("cache-clear") {
            let _ = crate::python::cache_clear(py, config);
        }
        let lf = enabled && config.get_flag("lf");
        let ff = enabled && config.get_flag("ff");
        let nf = enabled && config.get_flag("nf");
        let sw_reset = enabled && config.get_flag("sw-reset");
        let sw = enabled && (config.get_flag("sw") || config.get_flag("sw-skip") || sw_reset);
        let (sw_resume, sw_cache_error) = if sw && !sw_reset {
            crate::python::cache_stepwise(py, config)
        } else {
            (None, None)
        };
        let lastfailed = if enabled {
            crate::python::cache_lastfailed(py, config)
        } else {
            Vec::new()
        };
        let cached_nodeids = if enabled {
            crate::python::cache_nodeids(py, config)
        } else {
            Vec::new()
        };
        Self {
            enabled,
            lf,
            ff,
            nf,
            sw,
            sw_reset,
            sw_resume,
            sw_cache_error,
            lastfailed,
            cached_nodeids,
            skipped_files: 0,
            report_status: None,
            sw_statuses: Vec::new(),
            total_items: 0,
        }
    }

    /// LFPluginCollWrapper/CollSkipfiles: with --lf (and at least one
    /// previous failure still collected), files without failures skip
    /// entirely (counted), and top-level non-failed functions of failed
    /// files drop out — neither counts as "deselected". Files passed
    /// explicitly on the command line and class members are exempt: those
    /// deselect later in modify_items, like pytest's isinitpath/Collector
    /// exemptions.
    pub fn filter_collected_items(
        &mut self,
        rootdir: &Path,
        invocation_dir: &Path,
        paths: &[String],
        items: &mut Vec<TestItem>,
    ) {
        if !self.lf || self.lastfailed.is_empty() {
            return;
        }
        let failed_set: HashSet<&str> = self.lastfailed.iter().map(String::as_str).collect();
        if !items
            .iter()
            .any(|item| failed_set.contains(item.nodeid.as_str()))
        {
            return;
        }
        let failed_paths: HashSet<PathBuf> = self
            .lastfailed
            .iter()
            .filter_map(|nodeid| {
                let file = nodeid.split("::").next()?;
                let path = rootdir.join(file);
                let path = std::fs::canonicalize(&path).unwrap_or(path);
                path.is_file().then_some(path)
            })
            .collect();
        let arg_files: HashSet<PathBuf> = paths
            .iter()
            .map(|arg| {
                let arg = arg.split("::").next().unwrap_or(arg);
                let path = invocation_dir.join(arg);
                std::fs::canonicalize(&path).unwrap_or(path)
            })
            .filter(|path| path.is_file())
            .collect();
        let mut skipped: HashSet<PathBuf> = HashSet::new();
        items.retain(|item| {
            if arg_files.contains(&item.path) {
                return true;
            }
            if !failed_paths.contains(&item.path) {
                skipped.insert(item.path.clone());
                return false;
            }
            // Class members survive collection (the whole class node does);
            // they deselect in modify_items instead.
            let in_class = item
                .nodeid
                .split_once("::")
                .is_some_and(|(_, rest)| rest.contains("::"));
            failed_set.contains(item.nodeid.as_str()) || in_class
        });
        self.skipped_files = skipped.len();
    }

    /// LFPlugin/NFPlugin pytest_collection_modifyitems: --lf filters to the
    /// previous failures, --ff moves them first, --nf runs never-seen tests
    /// first (newest files first).
    pub fn modify_items(
        &mut self,
        config: &Config,
        items: &mut Vec<TestItem>,
        removed: &mut Vec<TestItem>,
    ) {
        if !self.enabled {
            return;
        }
        if self.lf || self.ff {
            if self.lastfailed.is_empty() {
                let mut status = "no previously failed tests, ".to_string();
                if self.lf
                    && config.get_value("last-failed-no-failures").map(str::trim) == Some("none")
                {
                    status.push_str("deselecting all items.");
                    removed.append(items);
                } else {
                    status.push_str("not deselecting items.");
                }
                self.report_status = Some(status);
            } else {
                let failed_set: HashSet<&str> =
                    self.lastfailed.iter().map(String::as_str).collect();
                let previously_failed = items
                    .iter()
                    .filter(|item| failed_set.contains(item.nodeid.as_str()))
                    .count();
                if previously_failed == 0 {
                    self.report_status = Some(format!(
                        "{} known failures not in selected tests",
                        self.lastfailed.len()
                    ));
                } else {
                    if self.lf {
                        let (kept, dropped): (Vec<TestItem>, Vec<TestItem>) = items
                            .drain(..)
                            .partition(|item| failed_set.contains(item.nodeid.as_str()));
                        *items = kept;
                        removed.extend(dropped);
                    } else {
                        // --ff: previous failures first, order otherwise kept.
                        let (failed, passed): (Vec<TestItem>, Vec<TestItem>) = items
                            .drain(..)
                            .partition(|item| failed_set.contains(item.nodeid.as_str()));
                        items.extend(failed);
                        items.extend(passed);
                    }
                    let noun = if previously_failed == 1 {
                        "failure"
                    } else {
                        "failures"
                    };
                    let suffix = if self.ff { " first" } else { "" };
                    self.report_status =
                        Some(format!("rerun previous {previously_failed} {noun}{suffix}"));
                }
                if self.skipped_files > 0 {
                    let files_noun = if self.skipped_files == 1 {
                        "file"
                    } else {
                        "files"
                    };
                    let status = self.report_status.get_or_insert_with(String::new);
                    status.push_str(&format!(" (skipped {} {})", self.skipped_files, files_noun));
                }
            }
        }
        // --stepwise status and item filtering.
        if self.sw {
            self.total_items = items.len();
            if let Some(err) = self.sw_cache_error.take() {
                self.sw_statuses.push(err);
            }
            if self.sw_reset {
                self.sw_statuses
                    .push("resetting state, not skipping.".to_string());
            } else if let Some((nodeid, last_count, age_str)) = &self.sw_resume {
                // Invalidate if test count changed since last run.
                let count_changed = last_count.map(|c| c != items.len()).unwrap_or(false);
                if count_changed {
                    self.sw_statuses.push(format!(
                        "test count changed, not skipping (now {} tests, previously {}).",
                        items.len(),
                        last_count.unwrap()
                    ));
                } else if let Some(index) = items.iter().position(|item| &item.nodeid == nodeid) {
                    let age = age_str.as_deref().unwrap_or("unknown");
                    self.sw_statuses.push(format!(
                        "skipping {index} already passed items (cache from {age} ago, use --sw-reset to discard)."
                    ));
                    removed.extend(items.drain(..index));
                } else {
                    self.sw_statuses
                        .push("previously failed test not found, not skipping.".to_string());
                }
            } else {
                self.sw_statuses
                    .push("no previously failed tests, not skipping.".to_string());
            }
        }
        if self.nf {
            let seen: HashSet<&str> = self.cached_nodeids.iter().map(String::as_str).collect();
            let (mut new_items, mut other_items): (Vec<TestItem>, Vec<TestItem>) = items
                .drain(..)
                .partition(|item| !seen.contains(item.nodeid.as_str()));
            // Newest files first within each group (mtime descending).
            let mtime_desc = |item: &TestItem| {
                std::cmp::Reverse(
                    item.path
                        .metadata()
                        .and_then(|meta| meta.modified())
                        .ok()
                        .map(|t| t.duration_since(std::time::UNIX_EPOCH).unwrap_or_default()),
                )
            };
            new_items.sort_by_key(mtime_desc);
            other_items.sort_by_key(mtime_desc);
            items.extend(new_items);
            items.extend(other_items);
        }
        // Remember every collected nodeid for the next --nf run.
        let seen: HashSet<&str> = self.cached_nodeids.iter().map(String::as_str).collect();
        let unseen: Vec<String> = items
            .iter()
            .filter(|item| !seen.contains(item.nodeid.as_str()))
            .map(|item| item.nodeid.clone())
            .collect();
        self.cached_nodeids.extend(unseen);
    }

    /// The "run-last-failure: ..." line shown after the collected count.
    pub fn status_line(&self, config: &Config) -> Option<String> {
        if !(self.lf || self.ff) || config.quiet {
            return None;
        }
        self.report_status
            .as_ref()
            .map(|status| format!("run-last-failure: {status}"))
    }

    /// The "stepwise: ..." lines shown after the collected count (may be >1 on error).
    pub fn stepwise_lines(&self, config: &Config) -> Vec<String> {
        if !self.sw || config.quiet || self.sw_statuses.is_empty() {
            return Vec::new();
        }
        self.sw_statuses
            .iter()
            .map(|status| format!("stepwise: {status}"))
            .collect()
    }

    /// LFPlugin/NFPlugin pytest_sessionfinish: replay the run's reports into
    /// cache/lastfailed and persist cache/nodeids.
    pub fn sessionfinish(
        &self,
        py: Python<'_>,
        config: &Config,
        reports: &[TestReport],
        items: &[TestItem],
    ) {
        if !self.enabled
            || config.is_worker()
            || crate::python::config_has_workerinput(py, config)
            || config.get_value("cache-show").is_some()
        {
            return;
        }
        let mut failed: Vec<String> = self.lastfailed.clone();
        let mut in_failed: HashSet<String> = failed.iter().cloned().collect();
        // A file that previously failed collection but now collects fine no
        // longer belongs in lastfailed under its bare file nodeid; its items
        // are tracked individually via the runtest reports below (upstream's
        // LFPlugin.pytest_collectreport pops the file, keeps the items).
        let collected_files: HashSet<&str> = items
            .iter()
            .map(|item| item.nodeid.split("::").next().unwrap_or(""))
            .collect();
        failed.retain(|nodeid| {
            let stale_collection_key =
                !nodeid.contains("::") && collected_files.contains(nodeid.as_str());
            if stale_collection_key {
                in_failed.remove(nodeid);
            }
            !stale_collection_key
        });
        for report in reports {
            let pop = (report.phase == Phase::Call && report.outcome == Outcome::Passed)
                || matches!(report.outcome, Outcome::Skipped | Outcome::XFailed);
            let push = report.outcome == Outcome::Failed;
            if pop {
                if in_failed.remove(&report.nodeid) {
                    failed.retain(|nodeid| nodeid != &report.nodeid);
                }
            } else if push && in_failed.insert(report.nodeid.clone()) {
                failed.push(report.nodeid.clone());
            }
        }
        let _ = crate::python::cache_write_session(py, config, &failed, &self.cached_nodeids);
        if self.sw {
            // Persist the first failing nodeid as the next resume point;
            // a fully passing run clears it.
            let first_failed = reports
                .iter()
                .find(|r| r.outcome == Outcome::Failed)
                .map(|r| r.nodeid.as_str());
            let count = if first_failed.is_some() {
                Some(self.total_items)
            } else {
                None
            };
            let _ = crate::python::cache_write_stepwise(py, config, first_failed, count);
        }
    }
}
