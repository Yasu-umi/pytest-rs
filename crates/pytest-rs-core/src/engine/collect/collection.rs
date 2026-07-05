use std::path::{Path, PathBuf};

use pyo3::prelude::*;

use super::super::Engine;
use crate::python;

impl Engine {
    pub(crate) fn fire_configure_and_print_header(
        &mut self,
        py: Python<'_>,
        rootdir: &Path,
        errors: &mut Vec<(PathBuf, String)>,
    ) -> Result<bool, String> {
        // pytest_load_initial_conftests (pytest-env sets os.environ here),
        // after option specs are registered so getini resolves, before configure.
        if let Err(err) = self.fire_py_load_initial_conftests(py) {
            // Errors here are fatal plugin-init failures (e.g. ImportError from a
            // bad DJANGO_SETTINGS_MODULE). Upstream lets them propagate to stderr
            // as a fatal error; we replicate that by printing to stderr and exiting.
            // UsageError is handled specially (exit code 4).
            if python::is_usage_error(py, &err) {
                let msg = python::format_exception(py, &err);
                let usage_msg = msg
                    .lines()
                    .last()
                    .and_then(|l| l.strip_prefix("pytest.UsageError: "))
                    .unwrap_or(msg.trim());
                eprintln!("ERROR: {usage_msg}");
                return Err("\x00USAGE_ERROR\x00".to_string());
            }
            eprintln!("{}", python::format_exception(py, &err));
            return Err("\x00USAGE_ERROR\x00".to_string());
        }
        // conftest pytest_configure hooks run once conftests are loaded.
        // Upstream fires this hook with no catch_warnings_for_item window at
        // all, so a warning raised directly in a conftest's pytest_configure
        // reaches whichever handler was already ambient (e.g. an outer
        // nested run's recwarn) instead of pytest's own capture. Re-apply the
        // ini/-W filters on reinstall too, or install()'s own defaults would
        // re-take priority over a user's filterwarnings override.
        let _ = python::suspend_warning_capture(py);
        let configure_result = self.fire_py_configure(py, rootdir, errors);
        let ini_filters: Vec<String> = self
            .config
            .get_ini_lines("filterwarnings")
            .into_iter()
            .map(str::to_string)
            .collect();
        let _ = python::install_warning_capture(py, &ini_filters, &self.config.w_options);
        if let Err(err) = configure_result {
            if python::is_usage_error(py, &err) {
                // UsageError in configure → eprintln ERROR: msg, then exit 4.
                let msg = python::format_exception(py, &err);
                // Extract just the exception message (drop "pytest.UsageError: " prefix).
                let usage_msg = msg
                    .lines()
                    .last()
                    .and_then(|l| l.strip_prefix("pytest.UsageError: "))
                    .unwrap_or(msg.trim());
                eprintln!("ERROR: {usage_msg}");
                return Err("\x00USAGE_ERROR\x00".to_string());
            }
            // pytest.exit() in pytest_configure: print "Exit: msg" to stderr
            // (no banner), return the requested exit code.
            if let Some(code) = python::session_abort_code(py, &err) {
                let exit_msg = err
                    .value(py)
                    .getattr("msg")
                    .and_then(|m| m.extract::<String>())
                    .unwrap_or_default();
                eprintln!("Exit: {exit_msg}");
                return Err(format!("\x00EXIT\x00{code}"));
            }
            // A non-UsageError, non-exit exception in pytest_configure is an
            // INTERNALERROR (exit 3) printed to stderr — upstream routes
            // configure failures to stderr (vs sessionstart on stdout). #49
            let msg = python::format_internal_error(py, &err, self.config.get_flag("full-trace"));
            return Err(format!("\x00INTERNAL_STDERR\x00{msg}"));
        }
        // A plugin instance registered in pytest_configure (#2270) may define
        // @pytest.fixture methods; register them as global fixtures bound to
        // the instance, so tests can request them.
        if let Err(err) = self.register_plugin_instance_fixtures(py) {
            errors.push((rootdir.to_path_buf(), python::format_exception(py, &err)));
        }
        // -h/--help: matches upstream's pytest_cmdline_main, which calls
        // showhelp() right after config._do_configure() and returns without
        // ever reaching wrap_session (no session header, no collection).
        if let Some(help_text) = &self.config.help_text {
            print!("{help_text}");
            print!("{}", crate::config::Config::HELP_FOOTER);
            // helpconfig.showhelp(): warnings recorded before this point (e.g.
            // a broken initial conftest, downgraded to a warning so --help
            // could still run) print as their own "warning : ..." lines.
            for line in python::showhelp_warning_lines(py) {
                println!("{line}");
            }
            return Ok(true);
        }
        // A plugin swapped in its own terminal reporter: suppress native
        // terminal output and drive the replacement object instead.
        if let Some(reporter) = python::reporter_replacement(py) {
            self.session.custom_reporter = Some(reporter);
            self.config.set_reporter_delegated();
        }
        // pytest_sessionstart python hooks fire before the header, like
        // upstream (the terminal's own sessionstart, which prints the header,
        // runs last under pluggy LIFO). A conftest sessionstart may stash
        // state the pytest_report_header hooks read back (e.g. config._x).
        if let Err(err) = self.fire_py_sessionstart(py) {
            // KeyboardInterrupt in pytest_sessionstart: same "!!! KeyboardInterrupt
            // !!!" banner (exit 2) as during collection, not the pytest.exit()
            // formatting below (which has no .msg attribute on KeyboardInterrupt
            // and would print a blank "Exit:  !!!" banner).
            if err.is_instance_of::<pyo3::exceptions::PyKeyboardInterrupt>(py) {
                return Err("\x00KEYBOARD_INTERRUPT\x00".to_string());
            }
            // pytest.exit() in pytest_sessionstart: print "Exit: msg" to stderr
            // AND show a "!!! Exit: msg !!!" banner in stdout.
            if let Some(code) = python::session_abort_code(py, &err) {
                let exit_msg = err
                    .value(py)
                    .getattr("msg")
                    .and_then(|m| m.extract::<String>())
                    .unwrap_or_default();
                eprintln!("Exit: {exit_msg}");
                self.session.abort_banner = Some(format!("Exit: {exit_msg}"));
                return Err(format!("\x00EXIT\x00{code}"));
            }
            // An unexpected exception in pytest_sessionstart is an INTERNALERROR
            // (exit 3), not a collection error (exit 2). Signal the caller with
            // a sentinel prefix so it can print the INTERNALERROR banner.
            let msg = python::format_internal_error(py, &err, self.config.get_flag("full-trace"));
            return Err(format!("\x00INTERNAL\x00{msg}"));
        }
        // The session header: the replacement reporter's pytest_sessionstart
        // owns it in delegated mode (upstream prints it from that hook);
        // otherwise the native header plus pytest_report_header lines
        // (e.g. pytest-timeout's "timeout: 1.0s" block).
        if self.session.custom_reporter.is_some() {
            python::reporter_sessionstart(py, &self.config);
        } else {
            self.print_header(py);
            if let Err(err) = self.print_py_report_header(py) {
                errors.push((rootdir.to_path_buf(), python::format_exception(py, &err)));
            }
        }
        // --markers: list registered markers (configure hooks above already
        // ran their addinivalue_line("markers", ...)) and skip collection.
        if self.config.get_flag("markers") {
            if let Err(err) = self.print_markers(py) {
                errors.push((rootdir.to_path_buf(), python::format_exception(py, &err)));
            }
            return Ok(true);
        }
        Ok(false)
    }

    /// Phase 6: apply collect_ignore / collect_ignore_glob / pytest_ignore_collect
    /// from loaded conftests as a post-filter over the candidate file set.
    pub(crate) fn apply_collect_ignores(
        &mut self,
        py: Python<'_>,
        rootdir: &Path,
        paths: &[String],
        conftests: &[PathBuf],
        files: &mut Vec<PathBuf>,
    ) {
        // Note: for explicit path args, pytest_ignore_collect is NOT called (upstream
        // "not called on argument" behaviour). collect_ignore is always applied.
        let no_explicit_file_args = paths.is_empty();
        // Gather ignore paths/globs from all loaded conftest modules.
        let mut extra_ignore_paths: Vec<std::path::PathBuf> = Vec::new();
        let mut extra_ignore_globs: Vec<String> = Vec::new();
        for conftest_path in conftests {
            if let Some(conftest_dir) = conftest_path.parent() {
                let (mut paths_from, mut globs_from) =
                    python::extract_collect_ignores(py, conftest_dir, conftest_path);
                extra_ignore_paths.append(&mut paths_from);
                extra_ignore_globs.append(&mut globs_from);
            }
        }
        if !extra_ignore_paths.is_empty() || !extra_ignore_globs.is_empty() {
            files.retain(|f| {
                let f_canonical = std::fs::canonicalize(f).unwrap_or_else(|_| f.clone());
                // collect_ignore: check if file or any ancestor is in the ignore list
                for ip in &extra_ignore_paths {
                    let ip_canonical = std::fs::canonicalize(ip).unwrap_or_else(|_| ip.clone());
                    if f_canonical.starts_with(&ip_canonical) || f_canonical == ip_canonical {
                        return false;
                    }
                }
                // collect_ignore_glob: check against full path
                if !extra_ignore_globs.is_empty() {
                    let f_str = f_canonical.to_string_lossy();
                    let f_name = f_canonical
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("");
                    for glob in &extra_ignore_globs {
                        if crate::collect::wildcard_match(glob, f_name)
                            || crate::collect::wildcard_match(glob, &f_str)
                        {
                            return false;
                        }
                    }
                }
                true
            });
        }
        // pytest_ignore_collect: only applied when no explicit file args
        if no_explicit_file_args {
            // Cache ignored/ok directories to avoid redundant hook calls.
            let mut known_ignored: std::collections::HashSet<PathBuf> = Default::default();
            let mut known_ok: std::collections::HashSet<PathBuf> = Default::default();

            let mut kept = Vec::with_capacity(files.len());
            for f in files.drain(..) {
                // Check ancestor directories: real pytest calls pytest_ignore_collect on
                // directories too, not just files. If a parent dir is ignored, all files
                // within it are skipped without inspecting them individually.
                let mut dir_ignored = false;
                let mut ancestor = f.parent().map(std::path::Path::to_path_buf);
                while let Some(ref d) = ancestor {
                    if d == rootdir {
                        break;
                    }
                    if known_ignored.contains(d) {
                        dir_ignored = true;
                        break;
                    }
                    if !known_ok.contains(d) {
                        if python::call_ignore_collect_hooks(py, &self.session.py_hooks, d, rootdir)
                            .is_some()
                        {
                            known_ignored.insert(d.clone());
                            dir_ignored = true;
                            break;
                        }
                        known_ok.insert(d.clone());
                    }
                    ancestor = d.parent().map(std::path::Path::to_path_buf);
                }
                if dir_ignored {
                    continue;
                }

                match python::call_ignore_collect_hooks(py, &self.session.py_hooks, &f, rootdir) {
                    None => kept.push(f),
                    Some(None) => {} // ignored silently
                    Some(Some(reason)) => {
                        // pytest.skip() in the hook: emit a skip report for this file
                        let nodeid = crate::collect::file_nodeid(rootdir, &f);
                        self.session.reports.push(crate::report::TestReport {
                            nodeid,
                            phase: crate::report::Phase::Setup,
                            outcome: crate::report::Outcome::Skipped,
                            duration: std::time::Duration::ZERO,
                            longrepr: Some(reason),
                            location: None,
                            subtest_desc: None,
                            sections: Vec::new(),
                            rerun: false,
                            xfail_longrepr: None,
                            reprcrash_message: None,
                            head_line: None,
                        });
                    }
                }
            }
            *files = kept;
        }
    }

    /// Phase 7: import each Python test file (recording collect errors /
    /// module-level skips), collecting its functions and `--doctest-modules`
    /// doctests; explicit non-Python files are routed to the doctest collector
    /// or returned as deferred (to be checked after custom collection runs).
    pub(crate) fn collect_files(
        &mut self,
        py: Python<'_>,
        rootdir: &Path,
        files: &[PathBuf],
        errors: &mut Vec<(PathBuf, String)>,
        skip_module_import: bool,
    ) -> Result<Vec<PathBuf>, String> {
        // pytest's catching_logs around pytest_collection: a root handler
        // during import keeps module-level logging calls from triggering
        // logging.basicConfig (issue #6240).
        let log_level_cfg: Option<String> = self
            .config
            .get_value("log-level")
            .map(str::to_string)
            .or_else(|| self.config.get_ini("log_level").map(str::to_string));
        python::log_start_phase(py, "collection", log_level_cfg.as_deref());
        // Expose pytest_pycollect_makeitem hooks to Python for collect_class.
        {
            use pyo3::types::PyAnyMethods;
            let makeitem_hooks: Vec<Py<PyAny>> = self
                .session
                .py_hooks
                .iter()
                .filter(|h| h.name == "pytest_pycollect_makeitem")
                .map(|h| h.func.clone_ref(py))
                .collect();
            let _ = py
                .import("pytest._node")
                .and_then(|m| m.call_method1("set_pycollect_hooks", (makeitem_hooks,)));
        }
        // pytest_collect_directory: conftest hooks may reject directories
        // (return None) to prevent collection. Pre-compute the set of rejected
        // dirs so files in them are skipped.
        let rejected_dirs: std::collections::HashSet<PathBuf> = {
            let mut rejected = std::collections::HashSet::new();
            if python::has_collect_directory_hook(py, &self.session.py_hooks) {
                let mut checked: std::collections::HashSet<PathBuf> = Default::default();
                for file in files {
                    if let Some(parent) = file.parent() {
                        let dir = parent.to_path_buf();
                        if checked.insert(dir.clone())
                            && let python::CollectDirResult::Skip =
                                python::call_collect_directory_hook(py, &dir, rootdir)
                        {
                            rejected.insert(dir);
                        }
                    }
                }
            }
            rejected
        };

        // Explicit non-Python, non-text-doctest file args that no collector handles.
        let mut not_found_files: Vec<PathBuf> = Vec::new();
        for file in files {
            if file
                .parent()
                .is_some_and(|parent| rejected_dirs.contains(parent))
            {
                continue;
            }
            // --maxfail aborts collection once the budget is spent on
            // collection errors, ignoring further files.
            if let Some(m) = self.config.maxfail()
                && errors.len() >= m
            {
                break;
            }
            let is_py = file.extension().and_then(|e| e.to_str()) == Some("py");
            if !is_py {
                // Non-Python files: only text files with doctest content.
                // For explicitly-specified files, collect regardless of --doctest-glob.
                // For scanned files, the glob loop below handles them.
                let ext = file.extension().and_then(|e| e.to_str()).unwrap_or("");
                let is_text_doctest = matches!(ext, "txt" | "rst" | "md");
                if is_text_doctest {
                    if let Ok(py_config) = python::make_py_config(py, &self.config)
                        && let Err(err) = python::collect_doctests_from_textfile(
                            py,
                            rootdir,
                            file,
                            &py_config,
                            &mut self.session.items,
                        )
                    {
                        errors.push((file.clone(), python::format_exception(py, &err)));
                    }
                } else {
                    // No collector can handle this file type (e.g. .pyc).
                    not_found_files.push(file.clone());
                }
                continue;
            }
            // In xdist spawn mode, workers collect modules themselves.
            // The controller only discovers file paths; importing here would
            // cause os._exit at module level to kill the controller.
            if skip_module_import {
                continue;
            }
            // Import-time output attaches to a failing collect report as
            // "Captured stdout/stderr" sections (pytest's
            // pytest_make_collect_report capture).
            python::capture_collect_begin(py);
            // Where this file's items start: --doctest-modules inserts the
            // module's doctest items BEFORE its functions (upstream order).
            let file_items_start = self.session.items.len();
            let collect_result = python::collect_module(
                py,
                rootdir,
                file,
                &mut self.session.items,
                &mut self.session.registry,
                &mut self.session.py_hooks,
                &python::NameFilters::from_config(py, &self.config),
                crate::collect::ImportMode::from_config(&self.config),
                &self.plugins,
            );
            let collect_sections = python::capture_collect_end(py);
            let with_sections = |mut message: String| {
                for (title, text) in &collect_sections {
                    message.push_str(&format!(
                        "\n{:-^80}\n{}",
                        format!(" {title} "),
                        text.trim_end_matches('\n')
                    ));
                }
                message
            };
            let module_ok = match collect_result {
                Ok(()) => true,
                Err(ref err) if err.is_instance_of::<pyo3::exceptions::PyKeyboardInterrupt>(py) => {
                    // KeyboardInterrupt during collection stops immediately with
                    // the "!!! KeyboardInterrupt !!!" banner (exit 2).
                    return Err("\x00KEYBOARD_INTERRUPT\x00".to_string());
                }
                Err(ref err) if err.is_instance_of::<pyo3::exceptions::PySystemExit>(py) => {
                    // SystemExit during collection is an INTERNALERROR.
                    let msg = python::format_exception(py, err);
                    return Err(format!("\x00INTERNAL\x00{msg}"));
                }
                Err(err) => {
                    // pytest.skip(..., allow_module_level=True) or
                    // unittest.SkipTest at module import skip the whole module;
                    // a bare pytest.skip there is an error.
                    match python::module_level_skip(py, &err) {
                        Some(Ok(reason)) => {
                            let nodeid = crate::collect::file_nodeid(rootdir, file);
                            // The skip call site (file:line), like pytest.
                            let location = python::raise_location(py, &err)
                                .unwrap_or_else(|| format!("{nodeid}:1"));
                            self.session.skipped_modules.push((
                                nodeid.clone(),
                                reason.clone(),
                                location.clone(),
                            ));
                            self.session.reports.push(crate::report::TestReport {
                                nodeid: nodeid.clone(),
                                phase: crate::report::Phase::Setup,
                                outcome: crate::report::Outcome::Skipped,
                                duration: std::time::Duration::ZERO,
                                longrepr: Some(reason),
                                location: Some(location),
                                subtest_desc: None,
                                sections: Vec::new(),
                                rerun: false,
                                xfail_longrepr: None,
                                reprcrash_message: None,
                                head_line: None,
                            });
                        }
                        Some(Err(message)) => errors.push((file.clone(), with_sections(message))),
                        // CollectError carries a user-facing message, no traceback.
                        None => {
                            match python::collect_error_message(py, &err) {
                                Some(message) => {
                                    errors.push((file.clone(), with_sections(message)))
                                }
                                None if python::is_import_error(py, &err) => {
                                    // A test module that fails to import gets
                                    // pytest's wrapped CollectError header
                                    // (importtestmodule), with a short-style
                                    // traceback. At -vv (verbosity >= 2) pytest
                                    // keeps its own framework frames in the
                                    // traceback; below that it filters them out.
                                    // We import via py.import (user frames only),
                                    // so re-import through import_path at -vv to
                                    // surface the `_pytest` frames faithfully.
                                    let tb = if self.config.global_verbosity() >= 2 {
                                        python::format_import_traceback_verbose(py, rootdir, file)
                                            .unwrap_or_else(|| {
                                                python::format_test_failure(py, &err, "short")
                                            })
                                    } else {
                                        python::format_test_failure(py, &err, "short")
                                    };
                                    let message = format!(
                                        "ImportError while importing test module '{}'.\n\
                                         Hint: make sure your test modules/packages have valid Python names.\n\
                                         Traceback:\n{tb}",
                                        file.display()
                                    );
                                    errors.push((file.clone(), with_sections(message)));
                                }
                                None => errors.push((
                                    file.clone(),
                                    // pytest-style frames + E lines (upstream
                                    // collect errors honor --tb; default "short"
                                    // matches pytest's auto style for collection).
                                    with_sections(python::format_test_failure(
                                        py,
                                        &err,
                                        self.config.get_value("tb").unwrap_or("short"),
                                    )),
                                )),
                            }
                            // Upstream DoctestModule: with --doctest-ignore-import-errors
                            // the doctest collector skips while the Module still errors.
                            if self.config.get_flag("doctest-modules")
                                && self.config.get_flag("doctest-ignore-import-errors")
                            {
                                let nodeid = crate::collect::file_nodeid(rootdir, file);
                                let longrepr = format!(
                                    "unable to import module PosixPath('{}')",
                                    file.display()
                                );
                                python::record_collect_skip(py, &nodeid, &longrepr);
                                self.session.reports.push(crate::report::TestReport {
                                    nodeid: nodeid.clone(),
                                    phase: crate::report::Phase::Setup,
                                    outcome: crate::report::Outcome::Skipped,
                                    duration: std::time::Duration::ZERO,
                                    longrepr: Some(longrepr),
                                    location: Some(format!("{nodeid}:1")),
                                    subtest_desc: None,
                                    sections: Vec::new(),
                                    rerun: false,
                                    xfail_longrepr: None,
                                    reprcrash_message: None,
                                    head_line: None,
                                });
                            }
                        }
                    }
                    false
                }
            };
            // --doctest-modules: collect doctests from each successfully-imported module.
            if module_ok
                && self.config.get_flag("doctest-modules")
                && let Ok(py_config) = python::make_py_config(py, &self.config)
            {
                let doctests_start = self.session.items.len();
                match python::collect_doctests_from_module(
                    py,
                    rootdir,
                    file,
                    &py_config,
                    &mut self.session.items,
                    crate::collect::ImportMode::from_config(&self.config),
                ) {
                    Ok(()) => {
                        // The module's doctests run BEFORE its functions
                        // (upstream collects the DoctestModule first).
                        let n_doctests = self.session.items.len().saturating_sub(doctests_start);
                        self.session.items[file_items_start..].rotate_right(n_doctests);
                    }
                    Err(err) => {
                        // Non-fatal: log as collect error and continue.
                        errors.push((file.clone(), python::format_exception(py, &err)));
                    }
                }
            }
        }

        // Return deferred non-Python files for the caller to check after
        // custom collection (collect_extra_and_custom) runs. Custom hooks like
        // pytest_collect_file may handle them; those that remain uncollected
        // will be reported as "not found" by the caller.
        Ok(not_found_files)
    }
}
