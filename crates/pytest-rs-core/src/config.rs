use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

/// Find and parse the pytest config file: walk up from `start` looking for
/// pytest.ini ([pytest]), pyproject.toml ([tool.pytest.ini_options]),
/// tox.ini ([pytest]) or setup.cfg ([tool:pytest]) — first hit wins and its
/// directory becomes the rootdir.
fn find_ini(start: &Path) -> (PathBuf, Option<String>, HashMap<String, String>) {
    for dir in start.ancestors() {
        let pytest_ini = dir.join("pytest.ini");
        if pytest_ini.exists()
            && let Ok(content) = std::fs::read_to_string(&pytest_ini)
        {
            // pytest.ini counts as config even with an empty/missing section.
            let values = parse_ini_section(&content, "pytest").unwrap_or_default();
            return (dir.to_path_buf(), Some("pytest.ini".to_string()), values);
        }
        let pyproject = dir.join("pyproject.toml");
        if pyproject.exists()
            && let Ok(content) = std::fs::read_to_string(&pyproject)
            && let Some(values) = parse_pyproject(&content)
        {
            return (
                dir.to_path_buf(),
                Some("pyproject.toml".to_string()),
                values,
            );
        }
        let tox_ini = dir.join("tox.ini");
        if tox_ini.exists()
            && let Ok(content) = std::fs::read_to_string(&tox_ini)
            && let Some(values) = parse_ini_section(&content, "pytest")
        {
            return (dir.to_path_buf(), Some("tox.ini".to_string()), values);
        }
        let setup_cfg = dir.join("setup.cfg");
        if setup_cfg.exists()
            && let Ok(content) = std::fs::read_to_string(&setup_cfg)
            && let Some(values) = parse_ini_section(&content, "tool:pytest")
        {
            return (dir.to_path_buf(), Some("setup.cfg".to_string()), values);
        }
    }
    (start.to_path_buf(), None, HashMap::new())
}

/// Minimal INI parser: one named section, `key = value` pairs, indented
/// continuation lines appended with newlines.
fn parse_ini_section(content: &str, section: &str) -> Option<HashMap<String, String>> {
    let mut values: HashMap<String, String> = HashMap::new();
    let mut in_section = false;
    let mut found = false;
    let mut current_key: Option<String> = None;
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            in_section = &trimmed[1..trimmed.len() - 1] == section;
            found |= in_section;
            current_key = None;
            continue;
        }
        if !in_section || trimmed.starts_with('#') || trimmed.starts_with(';') {
            continue;
        }
        if line.starts_with([' ', '\t']) && !trimmed.is_empty() {
            // continuation of a multiline value
            if let Some(key) = &current_key
                && let Some(value) = values.get_mut(key)
            {
                if !value.is_empty() {
                    value.push('\n');
                }
                value.push_str(trimmed);
            }
            continue;
        }
        if let Some((key, value)) = trimmed.split_once('=') {
            let key = key.trim().to_string();
            values.insert(key.clone(), value.trim().to_string());
            current_key = Some(key);
        }
    }
    found.then_some(values)
}

/// pytest config from pyproject.toml: [tool.pytest] (pytest 9 toml mode,
/// keys other than ini_options) or [tool.pytest.ini_options] (ini mode);
/// stringified pytest-style (arrays become newline-joined linelists).
/// Divergence: upstream errors when both styles are present; here toml
/// mode wins.
fn parse_pyproject(content: &str) -> Option<HashMap<String, String>> {
    let document: toml::Table = content.parse().ok()?;
    let tool_pytest = document.get("tool")?.get("pytest")?.as_table()?;
    let toml_mode: Vec<(&String, &toml::Value)> = tool_pytest
        .iter()
        .filter(|(key, _)| key.as_str() != "ini_options")
        .collect();
    let entries: Vec<(&String, &toml::Value)> = if !toml_mode.is_empty() {
        toml_mode
    } else {
        tool_pytest.get("ini_options")?.as_table()?.iter().collect()
    };
    let mut values = HashMap::new();
    for (key, value) in entries {
        let rendered = match value {
            toml::Value::String(s) => s.clone(),
            toml::Value::Array(items) => items
                .iter()
                .map(|item| match item {
                    toml::Value::String(s) => s.clone(),
                    other => other.to_string(),
                })
                .collect::<Vec<_>>()
                .join("\n"),
            other => other.to_string(),
        };
        values.insert(key.clone(), rendered);
    }
    Some(values)
}

/// A CLI option contributed by the core or a plugin.
#[derive(Debug, Clone)]
pub struct OptDef {
    pub name: String,
    pub takes_value: bool,
    pub default: Option<String>,
    pub help: String,
    /// The value may be omitted (`--cov` vs `--cov=src`); a bare occurrence
    /// records an empty string.
    pub optional_value: bool,
}

impl OptDef {
    pub fn flag(name: &str, help: &str) -> Self {
        Self {
            name: name.trim_start_matches("--").to_string(),
            takes_value: false,
            default: None,
            help: help.to_string(),
            optional_value: false,
        }
    }

    pub fn value(name: &str, default: Option<&str>, help: &str) -> Self {
        Self {
            name: name.trim_start_matches("--").to_string(),
            takes_value: true,
            default: default.map(str::to_string),
            help: help.to_string(),
            optional_value: false,
        }
    }

    pub fn optional_value(name: &str, help: &str) -> Self {
        Self {
            name: name.trim_start_matches("--").to_string(),
            takes_value: true,
            default: None,
            help: help.to_string(),
            optional_value: true,
        }
    }
}

/// Facade over the underlying arg parser so plugin crates do not depend on
/// a specific clap version.
#[derive(Debug, Default)]
pub struct OptionParser {
    opts: Vec<OptDef>,
}

impl OptionParser {
    pub fn add_option(&mut self, opt: OptDef) {
        self.opts.push(opt);
    }
}

/// Frozen CLI + ini configuration, immutable after parsing.
#[derive(Debug)]
pub struct Config {
    pub paths: Vec<String>,
    pub verbose: u8,
    pub quiet: bool,
    /// -q occurrences (-qq folds collect-only output per file).
    pub quiet_level: u8,
    pub exitfirst: bool,
    pub collect_only: bool,
    pub rootdir: PathBuf,
    /// The directory the runner was invoked from: relative CLI paths (and
    /// bare collection) resolve against this, not rootdir.
    pub invocation_dir: PathBuf,
    /// -W warning filter specs, applied at session start.
    pub w_options: Vec<String>,
    /// -p specs (e.g. `no:terminal` disables all terminal output).
    pub plugin_opts: Vec<String>,
    flags: HashSet<String>,
    values: HashMap<String, String>,
    /// -o name=value overrides; take precedence over file values.
    ini_overrides: HashMap<String, String>,
    /// Values from pytest.ini / pyproject.toml / tox.ini / setup.cfg.
    ini_file: HashMap<String, String>,
    /// The config file's basename, for the "configfile:" header line.
    pub config_file_name: Option<String>,
    /// Unknown `--flag[=value]` tokens deferred for python-plugin option
    /// specs (pytest_addoption): applied after plugin load, where clap
    /// cannot know them. Space-separated values are not supported.
    pub plugin_args: Vec<String>,
    /// A plugin replaced the 'terminalreporter' plugin during configure
    /// (pytest-sugar/pytest-pretty): native terminal output is suppressed
    /// and the engine drives the replacement object instead. Set once,
    /// after python pytest_configure hooks fire.
    reporter_delegated: std::sync::atomic::AtomicBool,
}

/// pytest's rootdir-discovery inputs: cwd plus every arg token that exists
/// on the filesystem (option values included; `::` node-id parts stripped).
fn dirs_from_args(cwd: &Path, argv: &[String]) -> Vec<PathBuf> {
    let mut dirs = vec![cwd.to_path_buf()];
    for arg in argv.iter().skip(1) {
        if arg.starts_with('-') {
            continue;
        }
        let candidate = arg.split("::").next().unwrap_or(arg);
        let path = Path::new(candidate);
        if !path.exists() {
            continue;
        }
        let path = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        dirs.push(if path.is_file() {
            path.parent().map(Path::to_path_buf).unwrap_or(path)
        } else {
            path
        });
    }
    dirs
}

fn common_ancestor(dirs: &[PathBuf]) -> PathBuf {
    let mut ancestor = dirs[0].clone();
    for dir in &dirs[1..] {
        while !dir.starts_with(&ancestor) {
            let Some(parent) = ancestor.parent() else {
                break;
            };
            ancestor = parent.to_path_buf();
        }
    }
    ancestor
}

/// Python shlex.split (posix): whitespace-separated tokens with '...'
/// (literal) and "..." (backslash escapes \\ and \") quoting.
fn shlex_split(input: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut in_word = false;
    let mut chars = input.chars();
    while let Some(c) = chars.next() {
        match c {
            c if c.is_whitespace() => {
                if in_word {
                    parts.push(std::mem::take(&mut current));
                    in_word = false;
                }
            }
            '\'' => {
                in_word = true;
                for inner in chars.by_ref() {
                    if inner == '\'' {
                        break;
                    }
                    current.push(inner);
                }
            }
            '"' => {
                in_word = true;
                while let Some(inner) = chars.next() {
                    match inner {
                        '"' => break,
                        '\\' => match chars.next() {
                            Some(esc @ ('"' | '\\')) => current.push(esc),
                            Some(other) => {
                                current.push('\\');
                                current.push(other);
                            }
                            None => current.push('\\'),
                        },
                        other => current.push(other),
                    }
                }
            }
            '\\' => {
                in_word = true;
                if let Some(esc) = chars.next() {
                    current.push(esc);
                }
            }
            other => {
                in_word = true;
                current.push(other);
            }
        }
    }
    if in_word {
        parts.push(current);
    }
    parts
}

impl Config {
    pub fn from_args(parser: OptionParser, argv: Vec<String>) -> Result<Self, String> {
        let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
        let cwd = std::fs::canonicalize(&cwd).unwrap_or(cwd);
        // Config-file search starts at the common ancestor of cwd and the
        // path-like args (pytest's rootdir algorithm); with no config file
        // anywhere, the ancestor itself is the rootdir.
        let ancestor = common_ancestor(&dirs_from_args(&cwd, &argv));
        let (rootdir, config_file_name, ini_file) = find_ini(&ancestor);

        // addopts from the config file are prepended to the CLI args.
        // `-o addopts=...` wins over the file: it must apply here, before
        // clap parsing, or the override could never disable addopts.
        let mut argv = argv;
        let mut override_addopts: Option<String> = None;
        let mut idx = 1;
        while idx < argv.len() {
            let arg = &argv[idx];
            let entry: Option<&str> = if arg == "-o" || arg == "--override-ini" {
                idx += 1;
                argv.get(idx).map(String::as_str)
            } else if let Some(rest) = arg.strip_prefix("--override-ini=") {
                Some(rest)
            } else if let Some(rest) = arg.strip_prefix("-o") {
                (!rest.is_empty()).then_some(rest)
            } else {
                None
            };
            if let Some(value) = entry.and_then(|entry| entry.strip_prefix("addopts=")) {
                override_addopts = Some(value.to_string());
            }
            idx += 1;
        }
        let addopts = override_addopts.or_else(|| ini_file.get("addopts").cloned());
        if let Some(addopts) = addopts {
            // shlex-style splitting: `-m "not performance"` is one argument.
            argv.splice(1..1, shlex_split(&addopts));
        }

        let mut cmd = clap::Command::new("pytest-rs")
            .disable_help_flag(false)
            .version(concat!(env!("CARGO_PKG_VERSION"), " (pytest-compatible)"))
            .arg(
                clap::Arg::new("paths")
                    .num_args(0..)
                    .value_name("FILE_OR_DIR"),
            )
            .arg(
                clap::Arg::new("verbose")
                    .short('v')
                    .long("verbose")
                    .action(clap::ArgAction::Count),
            )
            .arg(
                clap::Arg::new("quiet")
                    .short('q')
                    .long("quiet")
                    .action(clap::ArgAction::Count),
            )
            .arg(
                clap::Arg::new("exitfirst")
                    .short('x')
                    .long("exitfirst")
                    .action(clap::ArgAction::SetTrue),
            )
            .arg(
                clap::Arg::new("collect-only")
                    .long("collect-only")
                    .alias("co")
                    .action(clap::ArgAction::SetTrue),
            )
            .arg(
                clap::Arg::new("override-ini")
                    .short('o')
                    .long("override-ini")
                    .value_name("NAME=VALUE")
                    .action(clap::ArgAction::Append),
            )
            .arg(
                clap::Arg::new("pythonwarnings")
                    .short('W')
                    .long("pythonwarnings")
                    .value_name("WARNING")
                    .action(clap::ArgAction::Append),
            );

        // Core pytest options parsed into flags/values (queried via
        // get_flag/get_value); some are still inert and gain behavior as
        // features land.
        const CORE_FLAGS: [&str; 21] = [
            "markers", // list registered markers (ini + plugin-registered) and exit
            "strict-config",
            "strict-markers",
            "strict",
            "collect-in-virtualenv",
            "cache-clear",
            "no-header",
            "no-summary",
            "continue-on-collection-errors",
            "exact-mode", // placeholder; harmless
            "doctest-modules",
            "doctest-continue-on-failure",
            "doctest-ignore-import-errors",
            "nbmake",          // accepted-but-inert: notebook collection not implemented
            "worker",          // hidden: this process is a -n worker (IPC on stdin/stdout)
            "runxfail",        // report xfail-marked tests as if unmarked
            "setup-only",      // run fixtures, skip the tests
            "setup-plan",      // like --setup-only (fixtures do execute here)
            "setup-show",      // run tests, narrating fixture setup/teardown
            "traceconfig",     // accepted-but-inert: plugin trace header not implemented
            "keep-duplicates", // collect the same file once per duplicated arg
        ];
        const CORE_VALUES: [(&str, Option<char>); 38] = [
            ("confcutdir", None),
            ("deselect", None),
            ("log-level", None),
            ("log-format", None),
            ("log-date-format", None),
            ("log-cli-level", None),
            ("log-cli-format", None),
            ("log-cli-date-format", None),
            ("log-file", None),
            ("log-file-level", None),
            ("log-file-mode", None),
            ("log-file-format", None),
            ("log-file-date-format", None),
            ("log-disable", None),
            ("last-failed-no-failures", None),
            ("report-chars", Some('r')),
            ("markexpr", Some('m')),
            ("keyword", Some('k')),
            ("plugin", Some('p')),
            ("config-file", Some('c')),
            ("numprocesses", Some('n')),
            ("assert", None),
            ("tb", None),
            ("maxfail", None),
            ("durations", None),
            ("durations-min", None),
            ("color", None),
            ("basetemp", None),
            ("import-mode", None),
            ("capture", None),
            ("doctest-glob", None),
            ("doctest-report", None),
            ("ignore", None), // accepted-but-inert (conformance runs files explicitly)
            ("junit-xml", None), // JUnit XML report path (--junitxml alias)
            ("junit-prefix", None), // classname prefix (--junitprefix alias)
            ("dist", None),   // accepted-but-inert: module-affinity load is the only mode
            ("maxprocesses", None), // accepted-but-inert
            ("max-worker-restart", None), // accepted-but-inert: workers are not restarted
        ];
        // Without the xdist feature these options stay unregistered, so
        // `-n` is an unknown-option usage error, like pytest without the
        // pytest-xdist plugin installed.
        let xdist_only =
            |name: &str| matches!(name, "numprocesses" | "dist" | "maxprocesses" | "worker");
        let has_xdist = cfg!(feature = "xdist");
        for flag in CORE_FLAGS {
            if !has_xdist && xdist_only(flag) {
                continue;
            }
            cmd = cmd.arg(
                clap::Arg::new(flag)
                    .long(flag)
                    .action(clap::ArgAction::SetTrue)
                    .hide(true),
            );
        }
        cmd = cmd.arg(
            clap::Arg::new("capture-disable")
                .short('s')
                .action(clap::ArgAction::SetTrue)
                .hide(true),
        );
        // cacheprovider selection flags (each with its long-form alias).
        for (name, alias) in [
            ("lf", "last-failed"),
            ("ff", "failed-first"),
            ("nf", "new-first"),
            ("sw", "stepwise"),
            ("sw-skip", "stepwise-skip"),
            ("sw-reset", "stepwise-reset"),
        ] {
            cmd = cmd.arg(
                clap::Arg::new(name)
                    .long(name)
                    .alias(alias)
                    .action(clap::ArgAction::SetTrue)
                    .hide(true),
            );
        }
        cmd = cmd.arg(
            clap::Arg::new("cache-show")
                .long("cache-show")
                .value_name("GLOB")
                .num_args(0..=1)
                .default_missing_value("*")
                .action(clap::ArgAction::Append)
                .hide(true),
        );
        for (name, short) in CORE_VALUES.into_iter().chain([("rootdir-opt", None)]) {
            if !has_xdist && xdist_only(name) {
                continue;
            }
            let mut arg = clap::Arg::new(name)
                .value_name("VALUE")
                .action(clap::ArgAction::Append)
                .hide(true);
            arg = match name {
                "rootdir-opt" => arg.long("rootdir"),
                "last-failed-no-failures" => arg.long(name).alias("lfnf"),
                "junit-xml" => arg.long(name).alias("junitxml"),
                "junit-prefix" => arg.long(name).alias("junitprefix"),
                _ => arg.long(name),
            };
            if let Some(short) = short {
                arg = arg.short(short);
            }
            cmd = cmd.arg(arg);
        }

        for opt in &parser.opts {
            let arg = clap::Arg::new(opt.name.clone())
                .long(opt.name.clone())
                .help(opt.help.clone());
            let arg = if opt.takes_value {
                let arg = arg.action(clap::ArgAction::Append);
                let arg = if opt.optional_value {
                    arg.num_args(0..=1).default_missing_value("")
                } else {
                    arg
                };
                match &opt.default {
                    Some(d) => arg.default_value(d.clone()),
                    None => arg,
                }
            } else {
                arg.action(clap::ArgAction::SetTrue)
            };
            cmd = cmd.arg(arg);
        }

        // Long flags clap doesn't know are deferred for python-plugin
        // option specs (pytest_addoption runs after the interpreter is up).
        // Only the self-contained `--flag` / `--flag=value` forms are
        // recognized; unregistered leftovers usage-error at configure.
        let known_longs: HashSet<String> = cmd
            .get_arguments()
            .flat_map(|arg| {
                arg.get_long().into_iter().map(str::to_string).chain(
                    arg.get_all_aliases()
                        .into_iter()
                        .flatten()
                        .map(str::to_string),
                )
            })
            .collect();
        let mut plugin_args = Vec::new();
        let mut kept = Vec::new();
        let mut tokens = argv.into_iter().enumerate().peekable();
        while let Some((idx, token)) = tokens.next() {
            if idx == 0 || !token.starts_with("--") || token == "--" {
                kept.push(token);
                continue;
            }
            let name = token[2..].split('=').next().unwrap_or("");
            if known_longs.contains(name) {
                kept.push(token);
                continue;
            }
            // A separate value token (`--flag value`) is deferred with the
            // flag; whether the spec consumes it is decided at apply time.
            let space_value = !token.contains('=');
            plugin_args.push(token);
            if space_value
                && let Some((_, next)) = tokens.peek()
                && !next.starts_with('-')
            {
                plugin_args.push(tokens.next().expect("peeked").1);
            }
        }
        let argv = kept;

        let matches = match cmd.try_get_matches_from(argv) {
            Ok(matches) => matches,
            // --help/--version display and exit 0, like pytest (even when
            // combined with other options, e.g. --cache-show --help).
            Err(err)
                if matches!(
                    err.kind(),
                    clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion
                ) =>
            {
                let _ = err.print();
                std::process::exit(0);
            }
            Err(err) => return Err(err.to_string()),
        };

        let mut flags = HashSet::new();
        let mut values = HashMap::new();
        for opt in &parser.opts {
            if opt.takes_value {
                if let Some(parsed) = matches.get_many::<String>(&opt.name) {
                    let parsed: Vec<&str> = parsed.map(String::as_str).collect();
                    let stored = if opt.optional_value {
                        // Append semantics: occurrences newline-joined
                        // (split back via get_values).
                        parsed.join("\n")
                    } else {
                        // argparse semantics: the last occurrence wins.
                        (*parsed.last().expect("get_many is non-empty")).to_string()
                    };
                    values.insert(opt.name.clone(), stored);
                }
            } else if matches.get_flag(&opt.name) {
                flags.insert(opt.name.clone());
            }
        }
        for flag in CORE_FLAGS
            .into_iter()
            .chain(["capture-disable", "lf", "ff", "nf"])
        {
            if !has_xdist && xdist_only(flag) {
                continue;
            }
            if matches.get_flag(flag) {
                flags.insert(flag.to_string());
            }
        }
        if let Some(mut parsed) = matches.get_many::<String>("cache-show")
            && let Some(last) = parsed.next_back()
        {
            values.insert("cache-show".to_string(), last.clone());
        }
        let mut plugin_opts = Vec::new();
        for (name, _) in CORE_VALUES {
            if !has_xdist && xdist_only(name) {
                continue;
            }
            let Some(parsed) = matches.get_many::<String>(name) else {
                continue;
            };
            let parsed: Vec<&String> = parsed.collect();
            if name == "plugin" {
                plugin_opts = parsed.iter().map(|v| v.to_string()).collect();
            }
            if matches!(name, "deselect" | "doctest-glob" | "log-disable") {
                // Every occurrence matters (newline-joined for get_values).
                let joined = parsed
                    .iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join("\n");
                values.insert(name.to_string(), joined);
            } else if let Some(last) = parsed.last() {
                values.insert(name.to_string(), (*last).clone());
            }
        }

        let mut ini_overrides = HashMap::new();
        if let Some(overrides) = matches.get_many::<String>("override-ini") {
            for entry in overrides {
                if let Some((name, value)) = entry.split_once('=') {
                    ini_overrides.insert(name.to_string(), value.to_string());
                }
            }
        }

        Ok(Self {
            paths: matches
                .get_many::<String>("paths")
                .map(|vals| vals.cloned().collect())
                .unwrap_or_default(),
            verbose: matches.get_count("verbose"),
            quiet: matches.get_count("quiet") > 0,
            quiet_level: matches.get_count("quiet"),
            exitfirst: matches.get_flag("exitfirst"),
            collect_only: matches.get_flag("collect-only"),
            rootdir,
            invocation_dir: cwd,
            w_options: matches
                .get_many::<String>("pythonwarnings")
                .map(|vals| vals.cloned().collect())
                .unwrap_or_default(),
            plugin_opts,
            flags,
            values,
            ini_overrides,
            ini_file,
            config_file_name,
            plugin_args,
            reporter_delegated: std::sync::atomic::AtomicBool::new(false),
        })
    }

    /// An ini-style option: -o overrides win over config file values.
    pub fn get_ini(&self, name: &str) -> Option<&str> {
        self.ini_overrides
            .get(name)
            .or_else(|| self.ini_file.get(name))
            .map(String::as_str)
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
        self.get_ini("python_files")
            .map(|value| value.split_whitespace().map(str::to_string).collect())
            .filter(|patterns: &Vec<String>| !patterns.is_empty())
            .unwrap_or_else(|| vec!["test_*.py".to_string(), "*_test.py".to_string()])
    }

    /// norecursedirs ini patterns: directory basenames (fnmatch) skipped
    /// during collection recursion (pytest's defaults).
    pub fn norecursedirs_patterns(&self) -> Vec<String> {
        self.get_ini("norecursedirs")
            .map(|value| value.split_whitespace().map(str::to_string).collect())
            .filter(|patterns: &Vec<String>| !patterns.is_empty())
            .unwrap_or_else(|| {
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
            })
    }

    /// The effective failure budget: -x/--exitfirst means 1, otherwise the
    /// --maxfail=N value (0 disables, as in pytest).
    pub fn maxfail(&self) -> Option<usize> {
        if self.exitfirst {
            return Some(1);
        }
        self.get_value("maxfail")
            .and_then(|v| v.parse().ok())
            .filter(|&n| n > 0)
    }

    /// -n N (xdist-style): Some(N) requests distributed execution. "auto"
    /// and "logical" map to the CPU count; 0 means in-process.
    pub fn numprocesses(&self) -> Option<usize> {
        let value = self.get_value("numprocesses")?;
        let n = match value {
            "auto" | "logical" => std::thread::available_parallelism()
                .map(std::num::NonZero::get)
                .unwrap_or(1),
            other => other.parse().ok()?,
        };
        (n > 0).then_some(n)
    }
}
