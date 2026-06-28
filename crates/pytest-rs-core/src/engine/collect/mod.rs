//! Test collection: `Engine::collect` and its per-phase helpers.
//!
//! Collection runs as a sequence of phases — resolve paths, load plugins and
//! conftests, configure, filter, then collect modules/doctests/custom files and
//! finalize the item list. The orchestrator (`collect`) wires the phases; each
//! phase lives in its own helper so the control flow reads top-down.

use std::path::PathBuf;

use pyo3::prelude::*;

mod collection;
mod pipeline;
mod plugins;
mod reorder;

use super::Engine;
use crate::python;

impl Engine {
    pub(crate) fn collect(&mut self, py: Python<'_>) -> Result<Vec<(PathBuf, String)>, String> {
        let rootdir = self.config.rootdir.clone();
        let (paths, mut files) = self.resolve_collection_paths(py, &rootdir)?;
        self.load_cmdline_and_entrypoint_plugins(py)?;
        let (start_dirs, conftests) = self.discover_conftests(&rootdir, &paths, &files);

        let mut errors = Vec::new();
        self.load_and_validate_config(py, &rootdir, &paths, &start_dirs, &conftests, &mut errors)?;
        if self.fire_configure_and_print_header(py, &rootdir, &mut errors)? {
            // --markers (or another short-circuit) handled output; skip collection.
            return Ok(errors);
        }
        self.apply_collect_ignores(py, &rootdir, &paths, &conftests, &mut files);
        // Warnings issued during test collection are attributed to the "collect" phase.
        let _ = py
            .import("pytest._wcapture")
            .and_then(|m| m.call_method1("set_current_when", ("collect",)));
        // In xdist spawn mode, workers import test modules themselves.
        // Skip collect_module in the controller so os._exit at module level
        // cannot kill the controller process.
        #[cfg(feature = "xdist")]
        let skip_module_import = {
            // "-n0" means 0 workers (sequential) — do not skip imports.
            let using_xdist = self.config.numprocesses_spec().is_some_and(|s| s != "0")
                || self.config.get_flag("dist-load")
                || self.config.get_value("tx").is_some();
            using_xdist
                && !self.config.collect_only
                && !self.config.get_flag("fixtures")
                && !self.config.get_flag("fixtures-per-test")
        };
        #[cfg(not(feature = "xdist"))]
        let skip_module_import = false;

        let collect_result = (|| -> Result<(), String> {
            self.collect_files(py, &rootdir, &files, &mut errors, skip_module_import)?;
            self.collect_extra_and_custom(py, &rootdir, &paths, &files, &mut errors)?;
            if let Err(err) =
                python::validate_dynamic_fixture_scopes(py, &self.config, &self.session.registry)
            {
                let message = python::collect_error_message(py, &err)
                    .unwrap_or_else(|| python::format_exception(py, &err));
                errors.push((rootdir.clone(), message));
            }
            self.finalize_items(py, &rootdir, &paths)?;
            Ok(())
        })();
        // Reset to "config" phase after collection (covers sessionfinish/terminal_summary).
        let _ = py
            .import("pytest._wcapture")
            .and_then(|m| m.call_method1("set_current_when", ("config",)));
        collect_result?;
        Ok(errors)
    }
}
