use pyo3::prelude::*;

use super::super::Engine;
use super::super::center_banner;
use super::super::short_message;
use super::{format_g, format_trimmed, running_on_ci};
use crate::hooks::HookContext;
use crate::python;
use crate::report::{Outcome, Phase};

impl Engine {
    pub(crate) fn print_durations(&self) {
        let n: usize = match self.config.get_value("durations") {
            Some(v) => match v.parse() {
                Ok(n) => n,
                Err(_) => return,
            },
            None => return,
        };
        let explicit_min = self.config.get_value("durations-min");
        let verbose = self.config.global_verbosity() >= 2;
        let durations_min: f64 = explicit_min
            .and_then(|v| v.parse().ok())
            .unwrap_or(if verbose { 0.0 } else { 0.005 });

        let mut entries: Vec<(f64, &str, &str)> = self
            .session
            .reports
            .iter()
            .map(|r| {
                let phase = match r.phase {
                    Phase::Setup => "setup",
                    Phase::Call => "call",
                    Phase::Teardown => "teardown",
                };
                (r.duration.as_secs_f64(), phase, r.nodeid.as_str())
            })
            .collect();
        entries.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));

        if n > 0 {
            let banner = format!("slowest {n} durations");
            println!("{}", center_banner(&banner));
        } else {
            println!("{}", center_banner("slowest durations"));
        }

        let selected: Vec<_> = if n > 0 {
            entries.into_iter().take(n).collect()
        } else {
            entries
        };

        let mut hidden = 0;
        for &(secs, phase, nodeid) in &selected {
            if secs < durations_min {
                hidden += 1;
            } else {
                println!("{secs:.2}s {phase:8} {nodeid}");
            }
        }
        if hidden > 0 {
            let min_str = format_g(durations_min);
            let hint = if explicit_min.is_none() {
                format!(
                    "({hidden} durations < {min_str}s hidden.  Use -vv to show these durations.)"
                )
            } else {
                format!("({hidden} durations < {min_str}s hidden.)")
            };
            println!();
            println!("{hint}");
        }
    }

    /// The "short test summary info" section, controlled by -r chars.
    /// Groups print in the order the chars were given, matching pytest.
    pub(crate) fn print_short_summary(&self, py: Python<'_>) {
        let chars = self.report_chars();
        // Under verbosity_subtests == 0 pytest 9's builtin subtests plugin
        // returns ("", "", "") for non-failed subtests, so drop them from
        // the short summary too. The third-party pytest-subtests plugin has
        // no quiet mode, so this only applies when it is not active.
        // SUBFAILED (failed) always shows.
        let hide = self.config.quiet_subtests() && !python::has_subtests_plugin(py);

        let mut lines = Vec::new();
        for c in chars.chars() {
            if c == 's' {
                // --no-fold-skipped lists each skip on its own line, like the
                // FAILED lines: "SKIPPED nodeid - Skipped: reason" (upstream
                // show_skipped_unfolded uses the full longrepr reason, which we
                // store stripped of its "Skipped: " prefix — re-add it).
                if self.config.get_flag("no-fold-skipped") {
                    for report in &self.session.reports {
                        if report.outcome != Outcome::Skipped {
                            continue;
                        }
                        if hide && report.subtest_desc.is_some() {
                            continue;
                        }
                        let word = self.subtest_summary_word(py, report, "SKIPPED");
                        let mut line = format!(
                            "{word} {}",
                            crate::collect::cwd_relative_nodeid(
                                &self.config.rootdir,
                                &self.config.invocation_dir,
                                &report.nodeid
                            )
                        );
                        let reason = report.longrepr.clone().unwrap_or_default();
                        if !reason.is_empty() {
                            let reason = if reason.starts_with("Skipped") {
                                reason
                            } else {
                                format!("Skipped: {reason}")
                            };
                            line.push_str(&format!(" - {reason}"));
                        }
                        lines.push(line);
                    }
                    continue;
                }
                // Skips fold by (location, reason): "SKIPPED [2] file.py:3: x".
                // The section label comes from the first skipped report, so a
                // leading subtest skip relabels the whole group SUBSKIPPED[..]
                // (upstream show_skipped_folded uses skipped[0] for the word).
                let mut label: Option<String> = None;
                let mut skip_groups: Vec<((String, String), usize)> = Vec::new();
                for report in &self.session.reports {
                    if report.outcome != Outcome::Skipped {
                        continue;
                    }
                    if hide && report.subtest_desc.is_some() {
                        continue;
                    }
                    if label.is_none() {
                        label = Some(self.subtest_summary_word(py, report, "SKIPPED"));
                    }
                    let location = report.location.clone().unwrap_or_else(|| {
                        crate::collect::cwd_relative_nodeid(
                            &self.config.rootdir,
                            &self.config.invocation_dir,
                            &report.nodeid,
                        )
                    });
                    let reason = report.longrepr.clone().unwrap_or_default();
                    let key = (location, reason);
                    match skip_groups.iter_mut().find(|(group, _)| group == &key) {
                        Some((_, count)) => *count += 1,
                        None => skip_groups.push((key, 1)),
                    }
                }
                let label = label.unwrap_or_else(|| "SKIPPED".to_string());
                for ((location, reason), count) in skip_groups {
                    lines.push(format!("{label} [{count}] {location}: {reason}"));
                }
                continue;
            }
            for report in &self.session.reports {
                if hide && report.subtest_desc.is_some() && report.outcome != Outcome::Failed {
                    continue;
                }
                let word = match (c, report.phase, report.outcome) {
                    ('f', Phase::Call, Outcome::Failed) => "FAILED",
                    ('E', Phase::Setup | Phase::Teardown, Outcome::Failed) => "ERROR",
                    ('x', _, Outcome::XFailed) => "XFAIL",
                    ('X', _, Outcome::XPassed) => "XPASS",
                    // 'P' selects the PASSES section, not a summary line.
                    ('p', Phase::Call, Outcome::Passed) => "PASSED",
                    _ => continue,
                };
                let word = if report.subtest_desc.is_some() {
                    self.subtest_summary_word(py, report, word)
                } else {
                    word.to_string()
                };
                let mut line = format!(
                    "{word} {}",
                    crate::collect::cwd_relative_nodeid(
                        &self.config.rootdir,
                        &self.config.invocation_dir,
                        &report.nodeid
                    )
                );
                // Prefer reprcrash.message (always set, tb-style-independent)
                // over parsing longrepr lines (unavailable with --tb=no).
                // A collection SyntaxError has no reprcrash (upstream's
                // CollectErrorRepr lacks .reprcrash): don't re-derive the
                // message from longrepr in that case either, or the
                // reporting.rs suppression above gets silently undone. A
                // doctest failure has no reprcrash at all upstream (its
                // DocTestFailure/UnexpectedException repr never sets one, so
                // `rep.longrepr.reprcrash.message` raises AttributeError,
                // silently skipped) — override both sources so no summary
                // suffix appears, matching upstream's bare "FAILED nodeid".
                let is_doctest = self
                    .session
                    .items
                    .iter()
                    .find(|item| item.nodeid == report.nodeid)
                    .is_some_and(|item| item.is_doctest);
                let crash_msg = if is_doctest {
                    None
                } else {
                    report
                        .reprcrash_message
                        .as_deref()
                        .map(str::to_string)
                        .or_else(|| {
                            report
                                .longrepr
                                .as_deref()
                                .and_then(short_message)
                                .filter(|message| !message.starts_with("SyntaxError:"))
                        })
                };
                if let Some(message) = crash_msg {
                    // pytest's _get_line_with_reprcrash_message: failure/error
                    // lines show the full crash message on CI or at -vv, but
                    // otherwise (and always under --force-short-summary) trim it
                    // to the terminal width. xfail/xpass reasons are untrimmed.
                    let trimmable = matches!(c, 'f' | 'E' | 'p');
                    let full = (running_on_ci() || self.config.verbose >= 2)
                        && !self.config.get_flag("force-short-summary");
                    if !trimmable || full {
                        line.push_str(&format!(" - {message}"));
                    } else {
                        let available =
                            crate::runner::term_width().saturating_sub(line.chars().count());
                        if let Some(trimmed) = format_trimmed(" - ", &message, available) {
                            line.push_str(&trimmed);
                        }
                    }
                }
                lines.push(line);
            }
        }
        if lines.is_empty() {
            return;
        }
        println!("{}", center_banner("short test summary info"));
        for line in lines {
            println!("{line}");
        }
    }

    /// Resolve the summary word for a subtest report via the Python
    /// `pytest_report_teststatus` hook (which respects installed plugins
    /// like pytest-subtests). Falls back to the built-in format.
    fn subtest_summary_word(
        &self,
        py: Python<'_>,
        report: &crate::report::TestReport,
        fallback: &str,
    ) -> String {
        use crate::runner::report_teststatus;
        if let Some(status) = report_teststatus(py, &self.config, &self.session, report, None)
            && !status.word.is_empty()
        {
            return status.word;
        }
        let desc = report.subtest_desc.as_deref().unwrap_or("");
        match report.outcome {
            Outcome::Failed => format!("SUBFAILED{desc}"),
            Outcome::Passed => format!("SUBPASSED{desc}"),
            Outcome::Skipped => format!("SUBSKIPPED{desc}"),
            Outcome::XFailed => format!("SUBXFAILED{desc}"),
            _ => fallback.to_string(),
        }
    }

    pub(crate) fn print_plugin_summaries(
        &mut self,
        py: Python<'_>,
        exitstatus: i32,
    ) -> PyResult<()> {
        let mut out = String::new();
        let mut ctx = HookContext {
            py,
            session: &mut self.session,
            config: &self.config,
        };
        for plugin in &self.plugins {
            plugin.pytest_terminal_summary(&mut ctx, &mut out)?;
        }
        if !out.is_empty() {
            println!("{out}");
        }
        // conftest/plugin pytest_terminal_summary impls (native mode): the
        // terminalreporter kwarg is the default stand-in writing to stdout.
        // Delegated mode replays these from _reporter.finish instead.
        if self.session.custom_reporter.is_none() {
            let mut funcs: Vec<Py<PyAny>> =
                python::instance_hook_funcs(py, "pytest_terminal_summary");
            // Conftest-defined impls only here — third-party plugin modules
            // (e.g. pytest-rerunfailures) are already covered by
            // instance_hook_funcs above; session.py_hooks also carries an
            // entry for entry-point-loaded plugin modules (tagged
            // plugin_module = Some(name)), so including those here too
            // would print the same plugin's terminal summary section twice.
            funcs.extend(
                self.session
                    .py_hooks
                    .iter()
                    .filter(|hook| {
                        hook.name == "pytest_terminal_summary" && hook.plugin_module.is_none()
                    })
                    .map(|hook| hook.func.clone_ref(py)),
            );
            if !funcs.is_empty() {
                let reporter = py.import("pytest._reporter")?.getattr("_default")?;
                if !reporter.is_none() {
                    let config = python::make_py_config(py, &self.config)?;
                    let status: Py<PyAny> = exitstatus.into_pyobject(py)?.into_any().unbind();
                    for func in funcs {
                        if let Err(err) = python::call_py_hook(
                            py,
                            &func,
                            &[
                                ("terminalreporter", reporter.clone().unbind()),
                                ("exitstatus", status.clone_ref(py)),
                                ("config", config.clone_ref(py)),
                            ],
                        ) {
                            eprintln!("INTERNAL ERROR: {}", python::format_exception(py, &err));
                        }
                    }
                    // The stand-in buffers through a TerminalWriter on
                    // sys.stdout; flush it so a trailing write() with no
                    // newline (pytest-mypy's --mypy-xfail stdout dump) isn't
                    // stranded in the buffer when the engine exits.
                    if let Ok(stdout) = py.import("sys").and_then(|m| m.getattr("stdout")) {
                        let _ = stdout.call_method0("flush");
                    }
                }
            }
        }
        Ok(())
    }

    /// --markers: ini/plugin-registered marker lines first (each as
    /// `@pytest.mark.<line>`), then pytest's builtin markers verbatim.
    pub(crate) fn print_markers(&mut self, py: Python<'_>) -> PyResult<()> {
        // Read through the config proxy: configure-time
        // addinivalue_line("markers", ...) lands there, not in self.config.
        let config_proxy = python::make_py_config(py, &self.config)?;
        let value = config_proxy.bind(py).call_method1("getini", ("markers",))?;
        // getini("markers") is a linelist (upstream); tolerate a raw string.
        let registered: Vec<String> = value.extract::<Vec<String>>().or_else(|_| {
            value.extract::<Option<String>>().map(|raw| {
                raw.unwrap_or_default()
                    .lines()
                    .map(str::to_string)
                    .collect()
            })
        })?;
        for line in &registered {
            let line = line.trim();
            if !line.is_empty() {
                println!("@pytest.mark.{line}\n");
            }
        }
        for line in [
            "@pytest.mark.filterwarnings(warning): add a warning filter to the given test. see https://docs.pytest.org/en/stable/how-to/capture-warnings.html#pytest-mark-filterwarnings",
            "@pytest.mark.skip(reason=None): skip the given test function with an optional reason. Example: skip(reason=\"no way of currently testing this\") skips the test.",
            "@pytest.mark.skipif(condition, ..., *, reason=...): skip the given test function if any of the conditions evaluate to True. Example: skipif(sys.platform == 'win32') skips the test if we are on the win32 platform. See https://docs.pytest.org/en/stable/reference/reference.html#pytest-mark-skipif",
            "@pytest.mark.xfail(condition, ..., *, reason=..., run=True, raises=None, strict=strict_xfail): mark the test function as an expected failure if any of the conditions evaluate to True. Optionally specify a reason for better reporting and run=False if you don't even want to execute the test function. If only specific exception(s) are expected, you can list them in raises, and if the test fails in other ways, it will be reported as a true failure. See https://docs.pytest.org/en/stable/reference/reference.html#pytest-mark-xfail",
            "@pytest.mark.parametrize(argnames, argvalues): call a test function multiple times passing in different arguments in turn. argvalues generally needs to be a list of values if argnames specifies only one name or a list of tuples of values if argnames specifies multiple names. Example: @parametrize('arg1', [1,2]) would lead to two calls of the decorated test function, one with arg1=1 and another with arg1=2. see https://docs.pytest.org/en/stable/how-to/parametrize.html for more info and examples.",
            "@pytest.mark.usefixtures(fixturename1, fixturename2, ...): mark tests as needing all of the specified fixtures. see https://docs.pytest.org/en/stable/explanation/fixtures.html#usefixtures",
            "@pytest.mark.tryfirst: mark a hook implementation function such that the plugin machinery will try to call it first/as early as possible. DEPRECATED, use @pytest.hookimpl(tryfirst=True) instead.",
            "@pytest.mark.trylast: mark a hook implementation function such that the plugin machinery will try to call it last/as late as possible. DEPRECATED, use @pytest.hookimpl(trylast=True) instead.",
        ] {
            println!("{line}\n");
        }
        Ok(())
    }

    /// `--fixtures`: list every registered fixture (grouped by defining
    /// module), then exit. Rendering lives in the `pytest._showfixtures` shim,
    /// which derives location/docstring/module from each fixture's callable.
    pub(crate) fn show_fixtures(&mut self, py: Python<'_>) -> PyResult<()> {
        let defs = pyo3::types::PyList::empty(py);
        for def in self.session.registry.all_defs() {
            defs.append((
                def.name.as_str(),
                def.scope.as_str(),
                def.baseid.as_str(),
                def.func.clone_ref(py),
            ))?;
        }
        let invocation = self.config.invocation_dir.to_string_lossy();
        let verbose = self.config.global_verbosity();
        let text: String = py
            .import("pytest._showfixtures")?
            .call_method1(
                "show_fixtures",
                (defs, invocation.as_ref(), verbose, crate::tw::enabled()),
            )?
            .extract()?;
        if !text.is_empty() {
            println!("{text}");
        }
        Ok(())
    }

    /// `--fixtures-per-test`: for each collected item, list the fixtures in its
    /// closure, then exit.
    pub(crate) fn show_fixtures_per_test(&mut self, py: Python<'_>) -> PyResult<()> {
        let items_data = pyo3::types::PyList::empty(py);
        for item in &self.session.items {
            let mut direct: Vec<String> = item.fixture_names.clone();
            direct.extend(item.extra_fixture_names.iter().cloned());
            let ignore: std::collections::HashSet<String> =
                item.callspec.iter().map(|(name, _)| name.clone()).collect();
            let closure = self
                .session
                .registry
                .closure_for(&item.nodeid, &direct, &ignore);
            let closure_list = pyo3::types::PyList::empty(py);
            for def in &closure {
                closure_list.append((
                    def.name.as_str(),
                    def.scope.as_str(),
                    def.baseid.as_str(),
                    def.func.clone_ref(py),
                ))?;
            }
            let name = item.nodeid.rsplit("::").next().unwrap_or(&item.nodeid);
            items_data.append((name, item.func.clone_ref(py), closure_list))?;
        }
        let invocation = self.config.invocation_dir.to_string_lossy();
        let verbose = self.config.global_verbosity();
        let text: String = py
            .import("pytest._showfixtures")?
            .call_method1(
                "show_fixtures_per_test",
                (
                    items_data,
                    invocation.as_ref(),
                    verbose,
                    crate::tw::enabled(),
                ),
            )?
            .extract()?;
        if !text.is_empty() {
            println!("{text}");
        }
        Ok(())
    }

    /// `pytest_help_group` native-plugin hooks: each plugin with its own CLI
    /// option group (e.g. pytest-benchmark's `benchmark:` section) renders
    /// and returns its own text; printed as-is right after the core option
    /// listing, before `Config::HELP_FOOTER`.
    pub(crate) fn print_py_help_groups(&mut self, py: Python<'_>) -> PyResult<()> {
        let mut ctx = HookContext {
            py,
            session: &mut self.session,
            config: &self.config,
        };
        for plugin in &self.plugins {
            if let Some(text) = plugin.pytest_help_group(&mut ctx)? {
                print!("{text}");
            }
        }
        Ok(())
    }

    /// The native (Rust-implemented) plugins' pytest_report_header
    /// contributions (e.g. pytest-benchmark's "benchmark: ..." line) — these
    /// never reach Python, so a replacement terminalreporter (delegated
    /// mode) needs them passed in explicitly to match the native header.
    pub(crate) fn native_plugin_header_lines(&mut self, py: Python<'_>) -> PyResult<Vec<String>> {
        let mut ctx = HookContext {
            py,
            session: &mut self.session,
            config: &self.config,
        };
        let mut lines = Vec::new();
        for plugin in &self.plugins {
            for block in plugin.pytest_report_header(&mut ctx)? {
                for line in block.split('\n') {
                    lines.push(line.to_string());
                }
            }
        }
        Ok(lines)
    }

    /// pytest_report_header py hooks: each returns a str or list of str,
    /// printed under the session header.
    pub(crate) fn print_py_report_header(&mut self, py: Python<'_>) -> PyResult<()> {
        if self.config.quiet || self.config.no_terminal() {
            return Ok(());
        }
        for line in self.native_plugin_header_lines(py)? {
            println!("{line}");
        }
        let hook_funcs: Vec<Py<pyo3::PyAny>> = self
            .session
            .py_hooks
            .iter()
            .filter(|hook| hook.name == "pytest_report_header")
            .map(|hook| hook.func.clone_ref(py))
            .collect();
        if hook_funcs.is_empty() {
            return Ok(());
        }
        let config_proxy = python::make_py_config(py, &self.config)?;
        // pytest passes start_path (the rootdir) to report-header hooks that
        // declare it (kwarg-filtered by the callable's signature).
        let start_path = py
            .import("pathlib")?
            .getattr("Path")?
            .call1((self.config.rootdir.to_string_lossy().as_ref(),))?
            .unbind();
        for func in &hook_funcs {
            let result = python::call_py_hook(
                py,
                func,
                &[
                    ("config", config_proxy.clone_ref(py)),
                    ("start_path", start_path.clone_ref(py)),
                ],
            )?;
            let result = result.bind(py);
            if result.is_none() {
                continue;
            }
            let lines: Vec<String> = match result.extract::<String>() {
                Ok(line) => vec![line],
                Err(_) => result.extract().unwrap_or_default(),
            };
            for block in lines {
                for line in block.split('\n') {
                    println!("{line}");
                }
            }
        }
        Ok(())
    }

    /// pytest_report_collectionfinish py hooks: each returns a str or list
    /// of str, printed right after the "collected N items" line (the hook
    /// receives config, start_path and the collected item nodes).
    pub(crate) fn print_py_report_collectionfinish(&self, py: Python<'_>) -> PyResult<()> {
        if self.config.quiet || self.config.no_terminal() {
            return Ok(());
        }
        let hook_funcs: Vec<Py<pyo3::PyAny>> = self
            .session
            .py_hooks
            .iter()
            .filter(|hook| hook.name == "pytest_report_collectionfinish")
            .map(|hook| hook.func.clone_ref(py))
            .collect();
        if hook_funcs.is_empty() {
            return Ok(());
        }
        let config_proxy = python::make_py_config(py, &self.config)?;
        let start_path = py
            .import("pathlib")?
            .getattr("Path")?
            .call1((self.config.rootdir.to_string_lossy().as_ref(),))?
            .unbind();
        let items = pyo3::types::PyList::empty(py);
        for item in &self.session.items {
            items.append(python::make_node(py, item)?)?;
        }
        let items: Py<pyo3::PyAny> = items.into_any().unbind();
        for func in &hook_funcs {
            let result = python::call_py_hook(
                py,
                func,
                &[
                    ("config", config_proxy.clone_ref(py)),
                    ("start_path", start_path.clone_ref(py)),
                    ("items", items.clone_ref(py)),
                ],
            )?;
            let result = result.bind(py);
            if result.is_none() {
                continue;
            }
            let lines: Vec<String> = match result.extract::<String>() {
                Ok(line) => vec![line],
                Err(_) => result.extract().unwrap_or_default(),
            };
            for block in lines {
                for line in block.split('\n') {
                    println!("{line}");
                }
            }
        }
        Ok(())
    }
}
