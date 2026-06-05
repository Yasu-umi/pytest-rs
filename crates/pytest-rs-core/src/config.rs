use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

/// Find and parse the pytest config file: walk up from `start` looking for
/// pytest.ini ([pytest]), pyproject.toml ([tool.pytest.ini_options]),
/// tox.ini ([pytest]) or setup.cfg ([tool:pytest]) — first hit wins and its
/// directory becomes the rootdir.
fn find_ini(start: &Path) -> (PathBuf, HashMap<String, String>) {
    for dir in start.ancestors() {
        let pytest_ini = dir.join("pytest.ini");
        if pytest_ini.exists()
            && let Ok(content) = std::fs::read_to_string(&pytest_ini)
        {
            // pytest.ini counts as config even with an empty/missing section.
            let values = parse_ini_section(&content, "pytest").unwrap_or_default();
            return (dir.to_path_buf(), values);
        }
        let pyproject = dir.join("pyproject.toml");
        if pyproject.exists()
            && let Ok(content) = std::fs::read_to_string(&pyproject)
            && let Some(values) = parse_pyproject(&content)
        {
            return (dir.to_path_buf(), values);
        }
        let tox_ini = dir.join("tox.ini");
        if tox_ini.exists()
            && let Ok(content) = std::fs::read_to_string(&tox_ini)
            && let Some(values) = parse_ini_section(&content, "pytest")
        {
            return (dir.to_path_buf(), values);
        }
        let setup_cfg = dir.join("setup.cfg");
        if setup_cfg.exists()
            && let Ok(content) = std::fs::read_to_string(&setup_cfg)
            && let Some(values) = parse_ini_section(&content, "tool:pytest")
        {
            return (dir.to_path_buf(), values);
        }
    }
    (start.to_path_buf(), HashMap::new())
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

/// [tool.pytest.ini_options] from pyproject.toml, stringified pytest-style
/// (arrays become newline-joined linelists).
fn parse_pyproject(content: &str) -> Option<HashMap<String, String>> {
    let document: toml::Table = content.parse().ok()?;
    let options = document
        .get("tool")?
        .get("pytest")?
        .get("ini_options")?
        .as_table()?;
    let mut values = HashMap::new();
    for (key, value) in options {
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

impl Config {
    pub fn from_args(parser: OptionParser, argv: Vec<String>) -> Result<Self, String> {
        let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
        let cwd = std::fs::canonicalize(&cwd).unwrap_or(cwd);
        // Config-file search starts at the common ancestor of cwd and the
        // path-like args (pytest's rootdir algorithm); with no config file
        // anywhere, the ancestor itself is the rootdir.
        let ancestor = common_ancestor(&dirs_from_args(&cwd, &argv));
        let (rootdir, ini_file) = find_ini(&ancestor);

        // addopts from the config file are prepended to the CLI args.
        let mut argv = argv;
        if let Some(addopts) = ini_file.get("addopts") {
            let extra: Vec<String> = addopts.split_whitespace().map(str::to_string).collect();
            argv.splice(1..1, extra);
        }

        let mut cmd = clap::Command::new("pytest-rs")
            .disable_help_flag(false)
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
                    .action(clap::ArgAction::SetTrue),
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
        const CORE_FLAGS: [&str; 15] = [
            "strict-config",
            "strict-markers",
            "strict",
            "cache-clear",
            "no-header",
            "no-summary",
            "continue-on-collection-errors",
            "exact-mode",      // placeholder; harmless
            "doctest-modules", // accepted-but-inert: doctest collection not implemented
            "nbmake",          // accepted-but-inert: notebook collection not implemented
            "worker",          // hidden: this process is a -n worker (IPC on stdin/stdout)
            "runxfail",        // report xfail-marked tests as if unmarked
            "setup-only",      // run fixtures, skip the tests
            "setup-plan",      // like --setup-only (fixtures do execute here)
            "setup-show",      // run tests, narrating fixture setup/teardown
        ];
        const CORE_VALUES: [(&str, Option<char>); 21] = [
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
            ("doctest-glob", None),       // accepted-but-inert
            ("ignore", None),             // accepted-but-inert (conformance runs files explicitly)
            ("dist", None), // accepted-but-inert: module-affinity load is the only mode
            ("maxprocesses", None), // accepted-but-inert
            ("max-worker-restart", None), // accepted-but-inert: workers are not restarted
        ];
        for flag in CORE_FLAGS {
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
            let mut arg = clap::Arg::new(name)
                .value_name("VALUE")
                .action(clap::ArgAction::Append)
                .hide(true);
            arg = match name {
                "rootdir-opt" => arg.long("rootdir"),
                "last-failed-no-failures" => arg.long(name).alias("lfnf"),
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

        let matches = cmd.try_get_matches_from(argv).map_err(|e| e.to_string())?;

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
            if matches.get_flag(flag) {
                flags.insert(flag.to_string());
            }
        }
        if let Some(parsed) = matches.get_many::<String>("cache-show")
            && let Some(last) = parsed.last()
        {
            values.insert("cache-show".to_string(), last.clone());
        }
        let mut plugin_opts = Vec::new();
        for (name, _) in CORE_VALUES {
            let Some(parsed) = matches.get_many::<String>(name) else {
                continue;
            };
            let parsed: Vec<&String> = parsed.collect();
            if name == "plugin" {
                plugin_opts = parsed.iter().map(|v| v.to_string()).collect();
            }
            if let Some(last) = parsed.last() {
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
            quiet: matches.get_flag("quiet"),
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
    /// are always silent: their stdout is the IPC channel.
    pub fn no_terminal(&self) -> bool {
        self.is_worker()
            || self
                .plugin_opts
                .iter()
                .any(|spec| spec == "no:terminal" || spec == "no:terminalreporter")
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
