use super::types::Config;
use super::types::ProgressKind;
use std::collections::HashMap;

impl Config {
    /// The signed global verbosity level (pytest's `option.verbose`): -v
    /// raises it, -q lowers it, --verbosity=N sets it directly.
    pub fn global_verbosity(&self) -> i32 {
        self.verbose as i32 - self.quiet_level as i32
    }

    /// `-s` / `--capture=no`: output capturing is off.
    pub fn capture_disabled(&self) -> bool {
        self.get_flag("capture-disable") || self.get_value("capture") == Some("no")
    }

    /// The per-test progress field shown at the line edge (pytest's
    /// `_determine_show_progress_info` × console_output_style): a percentage,
    /// a count, a node duration, or hidden. Capturing-off hides it unless
    /// the style explicitly keeps it (#3038).
    pub fn progress_kind(&self) -> ProgressKind {
        let style = self.get_ini("console_output_style").unwrap_or("progress");
        if self.capture_disabled() && style != "progress-even-when-capture-no" {
            return ProgressKind::Hidden;
        }
        match style {
            "progress" | "progress-even-when-capture-no" => ProgressKind::Percent,
            "count" => ProgressKind::Count,
            "times" => ProgressKind::Times,
            // "classic" (and any unknown value) shows no progress field.
            _ => ProgressKind::Hidden,
        }
    }

    /// The verbosity for a fine-grained type (pytest's `get_verbosity`):
    /// the `verbosity_<type>` ini if set to an int, else the global level.
    /// The ini's "auto" default defers to the global level.
    pub fn verbosity_for(&self, ini_name: &str) -> i32 {
        match self.get_ini(ini_name) {
            Some(value) if value.trim() != "auto" => value
                .trim()
                .parse()
                .unwrap_or_else(|_| self.global_verbosity()),
            _ => self.global_verbosity(),
        }
    }

    /// Verbosity governing per-test progress display (pytest's
    /// VERBOSITY_TEST_CASES): >=1 shows a line per test, 0 groups chars by
    /// file, <0 shows bare chars.
    pub fn test_case_verbosity(&self) -> i32 {
        self.verbosity_for("verbosity_test_cases")
    }

    /// An ini-style option: -o overrides win over config file values.
    pub fn get_ini(&self, name: &str) -> Option<&str> {
        self.ini_overrides
            .get(name)
            .or_else(|| self.ini_file.get(name))
            .map(String::as_str)
    }

    /// A multi-value ini (linelist/args type): returns non-empty trimmed lines.
    /// Handles both `\n`-separated (traditional ini) and `\x00`-separated
    /// (TOML array sentinel, mirrors `_split_str` in Python's `_parser.py`).
    pub fn get_ini_lines(&self, name: &str) -> Vec<&str> {
        let Some(value) = self.get_ini(name) else {
            return vec![];
        };
        let sep = if value.contains('\x00') { '\x00' } else { '\n' };
        value
            .split(sep)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect()
    }

    /// The `[pytest]`-section keys read from the config file (excludes -o
    /// overrides), for unknown-config-option validation.
    pub fn ini_file_keys(&self) -> Vec<String> {
        self.ini_file.keys().cloned().collect()
    }

    /// A config-file boolean ini (pytest's _strtobool truthiness), or None
    /// when unset. Reads -o overrides then the file.
    pub fn ini_bool(&self, name: &str) -> Option<bool> {
        self.get_ini(name).map(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on" | "t" | "y"
            )
        })
    }

    /// All effective ini values (file merged with -o overrides).
    pub fn ini_snapshot(&self) -> HashMap<String, String> {
        let mut merged = self.ini_file.clone();
        merged.extend(
            self.ini_overrides
                .iter()
                .map(|(key, value)| (key.clone(), value.clone())),
        );
        merged
    }

    /// The raw -o override values (for alias-aware getini in the Python layer).
    pub fn ini_overrides_clone(&self) -> HashMap<String, String> {
        self.ini_overrides.clone()
    }

    /// The raw ini file values without -o overrides (for config._inicfg).
    pub fn ini_file_clone(&self) -> HashMap<String, String> {
        self.ini_file.clone()
    }

    /// Plugin-contributed boolean option.
    pub fn get_flag(&self, name: &str) -> bool {
        self.flags.contains(name.trim_start_matches("--"))
    }

    /// Plugin-contributed valued option.
    pub fn get_value(&self, name: &str) -> Option<&str> {
        self.values
            .get(name.trim_start_matches("--"))
            .map(String::as_str)
    }

    /// All occurrences of an append-style option (`OptDef::optional_value`),
    /// in CLI order; a bare occurrence is an empty string. None if absent.
    pub fn get_values(&self, name: &str) -> Option<Vec<&str>> {
        self.values
            .get(name.trim_start_matches("--"))
            .map(|joined| joined.split('\n').collect())
    }

    /// `-p no:terminal` disables the terminal reporter entirely. Workers
    /// are always silent: their stdout is the IPC channel. A python plugin
    /// replacing the 'terminalreporter' plugin also silences native output
    /// (the engine drives the replacement instead).
    pub fn no_terminal(&self) -> bool {
        self.is_worker()
            || self.reporter_delegated()
            || self
                .plugin_opts
                .iter()
                .any(|spec| spec == "no:terminal" || spec == "no:terminalreporter")
    }

    /// A python plugin owns terminal output (see `set_reporter_delegated`).
    pub fn reporter_delegated(&self) -> bool {
        self.reporter_delegated
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// no_terminal() minus reporter delegation: output a replacement
    /// reporter does NOT produce itself (the --collect-only tree is
    /// base-class behavior upstream reporter plugins inherit) still prints
    /// natively in delegated mode.
    pub fn no_terminal_explicit(&self) -> bool {
        self.is_worker()
            || self
                .plugin_opts
                .iter()
                .any(|spec| spec == "no:terminal" || spec == "no:terminalreporter")
    }

    /// Flip native terminal output off: a plugin registered its own
    /// 'terminalreporter' during pytest_configure.
    pub fn set_reporter_delegated(&self) {
        self.reporter_delegated
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }

    /// `-p no:NAME` for a bundled plugin.
    pub fn plugin_disabled(&self, name: &str) -> bool {
        self.plugin_opts
            .iter()
            .any(|spec| spec.strip_prefix("no:") == Some(name))
    }

    /// This process is a `-n` worker, driven over stdin/stdout.
    pub fn is_worker(&self) -> bool {
        self.get_flag("worker")
    }

    /// Mark this process as a worker after the fact: forked workers inherit
    /// the controller's parsed config, which lacks the spawn-only --worker
    /// flag, so plugins would otherwise take their controller code paths.
    pub fn mark_worker(&mut self) {
        self.flags.insert("worker".to_string());
    }

    /// python_files ini patterns (default test_*.py / *_test.py).
    pub fn python_files_patterns(&self) -> Vec<String> {
        let patterns: Vec<String> = self
            .get_ini_lines("python_files")
            .into_iter()
            .flat_map(|v| v.split_whitespace().map(str::to_string))
            .collect();
        if patterns.is_empty() {
            vec!["test_*.py".to_string(), "*_test.py".to_string()]
        } else {
            patterns
        }
    }

    /// python_classes ini patterns (default `Test`): a class name is a test
    /// class when it starts with, or fnmatch-globs, any pattern.
    pub fn python_classes_patterns(&self) -> Vec<String> {
        let patterns: Vec<String> = self
            .get_ini_lines("python_classes")
            .into_iter()
            .flat_map(|v| v.split_whitespace().map(str::to_string))
            .collect();
        if patterns.is_empty() {
            vec!["Test".to_string()]
        } else {
            patterns
        }
    }

    /// python_functions ini patterns (default `test`): a function/method name
    /// is a test when it starts with, or fnmatch-globs, any pattern.
    pub fn python_functions_patterns(&self) -> Vec<String> {
        let patterns: Vec<String> = self
            .get_ini_lines("python_functions")
            .into_iter()
            .flat_map(|v| v.split_whitespace().map(str::to_string))
            .collect();
        if patterns.is_empty() {
            vec!["test".to_string()]
        } else {
            patterns
        }
    }

    /// norecursedirs ini patterns: directory basenames (fnmatch) skipped
    /// during collection recursion (pytest's defaults).
    pub fn norecursedirs_patterns(&self) -> Vec<String> {
        let patterns: Vec<String> = self
            .get_ini_lines("norecursedirs")
            .into_iter()
            .flat_map(|v| v.split_whitespace().map(str::to_string))
            .collect();
        if !patterns.is_empty() {
            return patterns;
        }
        [
            "*.egg",
            ".*",
            "_darcs",
            "build",
            "CVS",
            "dist",
            "node_modules",
            "venv",
            "{arch}",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect()
    }

    /// The effective failure budget: explicit --maxfail=N takes precedence
    /// over -x/--exitfirst (which means 1). Returns None when unlimited.
    pub fn maxfail(&self) -> Option<usize> {
        // Explicit --maxfail=N overrides -x (pytest: --maxfail overrides exitfirst).
        let explicit = self
            .get_value("maxfail")
            .and_then(|v| v.parse().ok())
            .filter(|&n: &usize| n > 0);
        if explicit.is_some() {
            return explicit;
        }
        if self.exitfirst {
            return Some(1);
        }
        None
    }

    /// The raw -n value (xdist-style): "auto" / "logical" / a number.
    /// Resolution of auto/logical (psutil probe, env override, conftest
    /// pytest_xdist_auto_num_workers hooks) happens in the engine where
    /// the interpreter is available.
    pub fn numprocesses_spec(&self) -> Option<&str> {
        self.get_value("numprocesses")
    }

    /// --tx gateway specs expanded to one entry per worker: the worker's
    /// chdir directory (None for plain popen). "2*popen" repeats; only
    /// local popen gateways are supported.
    pub fn tx_worker_chdirs(&self) -> Option<Vec<Option<String>>> {
        let specs = self.get_values("tx")?;
        let mut workers = Vec::new();
        for spec in specs {
            let (count, rest) = match spec.split_once('*') {
                Some((n, rest)) => (n.parse::<usize>().unwrap_or(1), rest),
                None => (1, spec),
            };
            let mut parts = rest.split("//");
            if parts.next() != Some("popen") {
                continue; // ssh/socket gateways are not supported
            }
            let chdir = parts
                .filter_map(|attr| attr.strip_prefix("chdir="))
                .last()
                .map(str::to_string);
            for _ in 0..count {
                workers.push(chdir.clone());
            }
        }
        (!workers.is_empty()).then_some(workers)
    }

    /// --maxprocesses: caps -n auto / -n logical (not explicit -n N),
    /// like upstream xdist.
    pub fn maxprocesses(&self) -> Option<usize> {
        self.get_value("maxprocesses")
            .and_then(|v| v.parse().ok())
            .filter(|&n| n > 0)
    }
}
