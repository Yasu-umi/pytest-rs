use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

/// Find and parse the pytest config file: walk up from `start` looking for
/// pytest.ini ([pytest]), pyproject.toml ([tool.pytest.ini_options]),
/// tox.ini ([pytest]) or setup.cfg ([tool:pytest]) — first hit wins and its
/// directory becomes the rootdir.
/// pytest config file candidates, in pytest's locate_config priority order.
const CONFIG_NAMES: [&str; 7] = [
    "pytest.toml",
    ".pytest.toml",
    "pytest.ini",
    ".pytest.ini",
    "pyproject.toml",
    "tox.ini",
    "setup.cfg",
];

/// The pytest config dict from one candidate file, or None if the file is
/// absent or carries no pytest config (load_config_dict_from_file). The
/// pytest.toml/.pytest.toml and pytest.ini/.pytest.ini variants always count
/// as config when present, even when empty.
/// Returns Err for parse errors or pyproject.toml conflicts (UsageError),
/// Ok(None) when absent.
fn load_config(
    dir: &Path,
    name: &str,
) -> Result<Option<HashMap<String, String>>, String> {
    let Ok(content) = std::fs::read_to_string(dir.join(name)) else {
        return Ok(None);
    };
    let path = dir.join(name).to_string_lossy().into_owned();
    match name {
        "pytest.toml" | ".pytest.toml" => {
            let values = parse_toml_pytest(&content, Some(&path))?;
            Ok(Some(values.unwrap_or_default()))
        }
        "pytest.ini" | ".pytest.ini" => {
            if let Some(line) = detect_missing_section_header(&content) {
                return Err(format!("{}:{}: no section header defined", path, line));
            }
            Ok(Some(parse_ini_section(&content, "pytest").unwrap_or_default()))
        }
        "pyproject.toml" => parse_pyproject(&content, Some(&path)),
        "tox.ini" => Ok(parse_ini_section(&content, "pytest")),
        "setup.cfg" => Ok(parse_ini_section(&content, "tool:pytest")),
        _ => Ok(None),
    }
}

/// Detect a missing section header: the first non-blank, non-comment line
/// before any `[section]` that looks like a key=value pair. Returns the
/// 1-based line number on detection, None if the content is well-formed.
fn detect_missing_section_header(content: &str) -> Option<usize> {
    for (i, line) in content.lines().enumerate() {
        let t = line.trim();
        if t.is_empty() || t.starts_with('#') || t.starts_with(';') {
            continue;
        }
        if t.starts_with('[') {
            return None;
        }
        if t.contains('=') {
            return Some(i + 1);
        }
    }
    None
}

/// Locate the winning config file. Returns (rootdir, basename, values,
/// ignored), where `ignored` lists lower-priority config files in the same
/// directory that also hold pytest config — pytest warns about these.
///
/// Real pytest's `locate_config` fallback: if no file with pytest content is
/// found but a `pyproject.toml` exists anywhere in the walk, the closest one
/// (first encountered) becomes the inipath with an empty config dict.
/// Returns Err when a pyproject.toml conflict is detected (UsageError).
fn find_ini(
    start: &Path,
) -> Result<
    (
        PathBuf,
        Option<String>,
        HashMap<String, String>,
        Vec<String>,
    ),
    String,
> {
    let mut first_pyproject_dir: Option<PathBuf> = None;
    for dir in start.ancestors() {
        for (i, name) in CONFIG_NAMES.iter().enumerate() {
            if *name == "pyproject.toml"
                && first_pyproject_dir.is_none()
                && dir.join(name).exists()
            {
                first_pyproject_dir = Some(dir.to_path_buf());
            }
            if let Some(values) = load_config(dir, name)? {
                let ignored = CONFIG_NAMES[i + 1..]
                    .iter()
                    .filter(|other| {
                        load_config(dir, other).ok().flatten().is_some()
                    })
                    .map(|other| other.to_string())
                    .collect();
                return Ok((dir.to_path_buf(), Some(name.to_string()), values, ignored));
            }
        }
    }
    if let Some(dir) = first_pyproject_dir {
        return Ok((
            dir,
            Some("pyproject.toml".to_string()),
            HashMap::new(),
            Vec::new(),
        ));
    }
    Ok((start.to_path_buf(), None, HashMap::new(), Vec::new()))
}

/// Load pytest config from an explicit path (for -c/--config-file).
/// Returns Err on pyproject.toml conflicts; empty map on other failures.
/// Mirrors real pytest's `load_config_dict_from_file` dispatch rules:
/// - pytest.toml / .pytest.toml → `[pytest]` table
/// - any other .toml (including pyproject.toml and custom names) → `[tool.pytest.ini_options]`
/// - .cfg → `[tool:pytest]`
/// - .ini and everything else → `[pytest]`
fn load_config_from_path(path: &Path) -> Result<HashMap<String, String>, String> {
    let Ok(content) = std::fs::read_to_string(path) else {
        return Ok(HashMap::new());
    };
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    let path_str = path.to_string_lossy().into_owned();
    match path.extension().and_then(|e| e.to_str()) {
        Some("toml") => {
            if matches!(name, "pytest.toml" | ".pytest.toml") {
                Ok(parse_toml_pytest(&content, Some(&path_str))?.unwrap_or_default())
            } else {
                Ok(parse_pyproject(&content, Some(&path_str))?.unwrap_or_default())
            }
        }
        Some("cfg") => Ok(parse_ini_section(&content, "tool:pytest").unwrap_or_default()),
        _ => Ok(parse_ini_section(&content, "pytest").unwrap_or_default()),
    }
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

/// Compare two dotted version strings (e.g. "1.2.3") by major.minor.patch.
/// Returns true when `current` >= `required`.
fn semver_ge(current: &str, required: &str) -> bool {
    let parse = |v: &str| -> (u64, u64, u64) {
        let mut parts = v.split('.');
        let major = parts.next().and_then(|s| s.trim().parse().ok()).unwrap_or(0);
        let minor = parts.next().and_then(|s| s.trim().parse().ok()).unwrap_or(0);
        let patch = parts.next().and_then(|s| s.trim().parse().ok()).unwrap_or(0);
        (major, minor, patch)
    };
    parse(current) >= parse(required)
}

/// pytest config from pyproject.toml: [tool.pytest] (pytest 9 toml mode,
/// keys other than ini_options) or [tool.pytest.ini_options] (ini mode).
/// Returns Err when both styles are present simultaneously (upstream
/// UsageError), Ok(None) when no pytest config is found.
fn parse_pyproject(content: &str, path: Option<&str>) -> Result<Option<HashMap<String, String>>, String> {
    let document: toml::Table = match content.parse() {
        Ok(t) => t,
        Err(e) => {
            return match path {
                Some(p) => Err(format!("{p}: Invalid statement ({e})")),
                None => Ok(None),
            };
        }
    };
    let Some(tool) = document.get("tool") else {
        return Ok(None);
    };
    let Some(tool_pytest) = tool.get("pytest").and_then(|v| v.as_table()) else {
        return Ok(None);
    };
    let has_native: bool = tool_pytest.keys().any(|k| k != "ini_options");
    let has_ini_options: bool = tool_pytest.contains_key("ini_options");
    if has_native && has_ini_options {
        return Err(
            "Cannot use both [tool.pytest.ini_options] and [tool.pytest] \
             for pytest configuration in pyproject.toml"
                .to_string(),
        );
    }
    let entries: Vec<(&String, &toml::Value)> = if has_native {
        tool_pytest.iter().collect()
    } else {
        match tool_pytest.get("ini_options").and_then(|v| v.as_table()) {
            Some(t) => t.iter().collect(),
            None => return Ok(None),
        }
    };
    Ok(Some(render_toml_entries(entries)))
}

/// pytest config from a standalone pytest.toml / .pytest.toml: a top-level
/// `[pytest]` table (pytest 9 toml mode). Returns None when no `[pytest]`
/// table is present (the caller still treats the file as config, just empty).
fn parse_toml_pytest(content: &str, path: Option<&str>) -> Result<Option<HashMap<String, String>>, String> {
    let document: toml::Table = match content.parse() {
        Ok(t) => t,
        Err(e) => {
            return match path {
                Some(p) => Err(format!("{p}: Invalid statement ({e})")),
                None => Ok(None),
            };
        }
    };
    let Some(pytest) = document.get("pytest").and_then(|v| v.as_table()) else {
        return Ok(None);
    };
    Ok(Some(render_toml_entries(pytest.iter().collect())))
}

/// Render TOML pytest entries into the engine's stringified ini form: scalar
/// values become their string, arrays become newline-joined linelists.
fn render_toml_entries(entries: Vec<(&String, &toml::Value)>) -> HashMap<String, String> {
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
                // NUL-byte sentinel: signals to _coerce_ini that this is a
                // pre-split TOML array (items may contain spaces), not an
                // ini-file string that needs shlex.split().
                .join("\x00"),
            other => other.to_string(),
        };
        values.insert(key.clone(), rendered);
    }
    values
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

/// The console_output_style progress field rendered at the right edge of a
/// progress line (pytest's `_show_progress_info`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgressKind {
    /// "[ 50%]" (the "progress" style, default).
    Percent,
    /// "[10/20]" (the "count" style).
    Count,
    /// A per-file node duration (the "times" style).
    Times,
    /// No field at all ("classic", capture-off, or unknown styles).
    Hidden,
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
    pub(crate) ini_overrides: HashMap<String, String>,
    /// Values from pytest.ini / pyproject.toml / tox.ini / setup.cfg.
    ini_file: HashMap<String, String>,
    /// The config file's basename, for the "configfile:" header line.
    pub config_file_name: Option<String>,
    /// Lower-priority config files in the rootdir that also hold pytest config
    /// but were ignored — pytest appends a "(WARNING: ignoring ...)" note.
    pub ignored_config_files: Vec<String>,
    /// The full argument list after addopts splicing — for plugins with
    /// position-sensitive options (pytest-cov's --cov-reset / --no-cov).
    pub effective_args: Vec<String>,
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

/// pytest's rootdir-discovery inputs: explicit filesystem path args, falling
/// back to cwd when no path args exist (mirrors pytest's get_dirs_from_args).
fn dirs_from_args(cwd: &Path, argv: &[String]) -> Vec<PathBuf> {
    let mut dirs = vec![];
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
    if dirs.is_empty() {
        dirs.push(cwd.to_path_buf());
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
/// A token is a positional test-path arg if it looks like a filesystem path
/// (contains `/`, `\`, or a Python-file extension). Used to avoid greedily
/// consuming test-path positionals as values for deferred plugin flags.
fn looks_like_path(s: &str) -> bool {
    s.contains('/') || s.contains('\\') || s.ends_with(".py") || s.ends_with(".txt")
        || s.ends_with(".toml") || s.ends_with(".cfg") || s.ends_with(".ini")
        || s.starts_with('.') // ./relative or ../up
}

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
        // Pre-scan argv for -c/--config-file; when present, use the explicit
        // path directly instead of auto-discovery (pytest's inifile= path).
        let explicit_config: Option<String> = {
            let mut found = None;
            let mut i = 1;
            while i < argv.len() {
                let arg = &argv[i];
                if (arg == "-c" || arg == "--config-file") && i + 1 < argv.len() {
                    found = Some(argv[i + 1].clone());
                    break;
                } else if let Some(rest) = arg.strip_prefix("--config-file=") {
                    found = Some(rest.to_string());
                    break;
                }
                i += 1;
            }
            found
        };

        // Config-file search starts at the common ancestor of cwd and the
        // path-like args (pytest's rootdir algorithm); with no config file
        // anywhere, the ancestor itself is the rootdir.
        let (rootdir, config_file_name, ini_file, ignored_config_files) =
            if let Some(cf_arg) = explicit_config {
                let cf_path = if Path::new(&cf_arg).is_absolute() {
                    PathBuf::from(&cf_arg)
                } else {
                    cwd.join(&cf_arg)
                };
                let ini_file = load_config_from_path(&cf_path)?;
                let rootdir = cf_path
                    .parent()
                    .map(|p| p.to_path_buf())
                    .unwrap_or_else(|| cwd.clone());
                let file_name = cf_path
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string());
                (rootdir, file_name, ini_file, Vec::new())
            } else {
                let ancestor = common_ancestor(&dirs_from_args(&cwd, &argv));
                find_ini(&ancestor)?
            };

        // --rootdir=DIR must point at an existing directory (upstream
        // determine_setup raises a UsageError otherwise).
        let rootdir_arg = {
            let mut value = None;
            let mut iter = argv.iter();
            while let Some(arg) = iter.next() {
                if let Some(rest) = arg.strip_prefix("--rootdir=") {
                    value = Some(rest.to_string());
                } else if arg == "--rootdir" {
                    value = iter.next().cloned();
                }
            }
            value
        };
        if let Some(arg) = &rootdir_arg {
            let path = cwd.join(arg);
            if !path.is_dir() {
                return Err(format!(
                    "Directory '{}' not found. Check your '--rootdir' option.",
                    path.display()
                ));
            }
        }

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
        // PYTEST_ADDOPTS env args sit between ini addopts and the CLI
        // (upstream: ini addopts, then env, then command line).
        if let Ok(env_addopts) = std::env::var("PYTEST_ADDOPTS")
            && !env_addopts.trim().is_empty()
        {
            argv.splice(1..1, shlex_split(&env_addopts));
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
                // pytest's --verbosity=N sets the global verbose level
                // directly (may be negative, so allow a leading hyphen).
                clap::Arg::new("verbosity")
                    .long("verbosity")
                    .value_name("VERBOSE")
                    .allow_hyphen_values(true)
                    .num_args(1),
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
        const CORE_FLAGS: [&str; 25] = [
            "force-short-summary", // truncate short-summary messages even at -vv
            "no-fold-skipped",     // list each skipped test in the short summary
            "xfail-tb",            // show tracebacks for xfailed tests in XFAILURES
            "no-showlocals",       // overrides an addopts --showlocals / -l
            "markers",             // list registered markers (ini + plugin-registered) and exit
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
        const CORE_VALUES: [(&str, Option<char>); 42] = [
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
            ("show-capture", None), // which captured sections to show on failure
            ("doctest-glob", None),
            ("doctest-report", None),
            ("ignore", None),             // paths pruned from collection
            ("ignore-glob", None),        // fnmatch patterns pruned from collection
            ("junit-xml", None),          // JUnit XML report path (--junitxml alias)
            ("junit-prefix", None),       // classname prefix (--junitprefix alias)
            ("dist", None), // accepted-but-inert: module-affinity load is the only mode
            ("maxprocesses", None), // accepted-but-inert
            ("max-worker-restart", None), // accepted-but-inert: workers are not restarted
            ("tx", None),   // xdist gateway specs ("2*popen", "popen//chdir=DIR")
            ("rsyncdir", None), // accepted-but-inert: fork workers share the filesystem
        ];
        // Without the xdist feature these options stay unregistered, so
        // `-n` is an unknown-option usage error, like pytest without the
        // pytest-xdist plugin installed.
        let xdist_only = |name: &str| {
            matches!(
                name,
                "numprocesses" | "dist" | "maxprocesses" | "worker" | "tx" | "rsyncdir"
            )
        };
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
        if has_xdist {
            // xdist's `-d`: distribute with the default load scheduler.
            cmd = cmd.arg(
                clap::Arg::new("dist-load")
                    .short('d')
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
        // -l / --showlocals: show local variables in tracebacks.
        cmd = cmd.arg(
            clap::Arg::new("showlocals")
                .short('l')
                .long("showlocals")
                .action(clap::ArgAction::SetTrue)
                .hide(true),
        );
        // --fulltrace: don't cut any tracebacks (accepted; read as the
        // `fulltrace` option by the config proxy).
        cmd = cmd.arg(
            clap::Arg::new("full-trace")
                .long("fulltrace")
                .action(clap::ArgAction::SetTrue)
                .hide(true),
        );
        // cacheprovider selection flags (each with its long-form alias).
        for (name, alias) in [
            ("lf", "last-failed"),
            ("ff", "failed-first"),
            ("nf", "new-first"),
            ("sw", "stepwise"),
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
            clap::Arg::new("sw-skip")
                .long("sw-skip")
                .alias("stepwise-skip")
                .action(clap::ArgAction::SetTrue)
                .help("Ignore the first failing test but stop on the next failing test. Implicitly enables --stepwise."),
        );
        cmd = cmd.arg(
            clap::Arg::new("cache-show")
                .long("cache-show")
                .value_name("GLOB")
                .num_args(0..=1)
                .default_missing_value("*")
                .action(clap::ArgAction::Append)
                .hide(true),
        );
        cmd = cmd.arg(
            clap::Arg::new("debug")
                .long("debug")
                .value_name("DEBUG_FILE_NAME")
                .num_args(0..=1)
                .default_missing_value("pytestdebug.log")
                .action(clap::ArgAction::Append)
                .help("Store internal tracing debug information in this log\nfile. This file is opened with 'w' and truncated as a\nresult.\nDefault: pytestdebug.log."),
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
            // Skip the peek for path-like tokens (test file args): they're
            // positional args for clap, not values for the deferred flag.
            let space_value = !token.contains('=');
            plugin_args.push(token);
            if space_value
                && let Some((_, next)) = tokens.peek()
                && !next.starts_with('-')
                && !looks_like_path(next)
            {
                plugin_args.push(tokens.next().expect("peeked").1);
            }
        }
        let argv = kept;

        let effective_args = argv.clone();
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
            Err(err) => {
                let msg = err.to_string();
                // Rewrite clap's verbose error for --override-ini missing value
                // to match pytest's argparse-style: "*: error: argument -o/--override-ini: ..."
                if msg.contains("override-ini")
                    && (msg.contains("required") || msg.contains("value"))
                {
                    return Err(
                        "pytest: error: argument -o/--override-ini: expected one argument"
                            .to_string(),
                    );
                }
                return Err(msg);
            }
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
        for flag in CORE_FLAGS.into_iter().chain([
            "capture-disable",
            "showlocals",
            "lf",
            "ff",
            "nf",
            "sw",
            "sw-skip",
            "sw-reset",
        ]) {
            if !has_xdist && xdist_only(flag) {
                continue;
            }
            if matches.get_flag(flag) {
                flags.insert(flag.to_string());
            }
        }
        if has_xdist && matches.get_flag("dist-load") {
            flags.insert("dist-load".to_string());
        }
        if let Some(mut parsed) = matches.get_many::<String>("cache-show")
            && let Some(last) = parsed.next_back()
        {
            values.insert("cache-show".to_string(), last.clone());
        }
        if let Some(mut parsed) = matches.get_many::<String>("debug")
            && let Some(last) = parsed.next_back()
        {
            values.insert("debug".to_string(), last.clone());
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
            if matches!(
                name,
                "deselect"
                    | "doctest-glob"
                    | "log-disable"
                    | "tx"
                    | "rsyncdir"
                    | "ignore"
                    | "ignore-glob"
            ) {
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
                // pytest rejects bare names: -o/--override-ini wants
                // option=value, not a lone option.
                let Some((name, value)) = entry.split_once('=') else {
                    return Err(format!(
                        "-o/--override-ini expects option=value style (got: '{entry}')."
                    ));
                };
                ini_overrides.insert(name.to_string(), value.to_string());
            }
        }

        // --confcutdir must point at an existing directory (upstream raises UsageError).
        if let Some(dir_val) = values.get("confcutdir") {
            let dir_path = if std::path::Path::new(dir_val).is_absolute() {
                std::path::PathBuf::from(dir_val)
            } else {
                cwd.join(dir_val)
            };
            if !dir_path.is_dir() {
                return Err(format!(
                    "--confcutdir must be a directory, given: {dir_val}"
                ));
            }
        }

        // minversion check: if the config requires a newer pytest than ours.
        // Compare against the pytest API version we track (9.0.3), not the
        // pytest-rs package version (0.0.3), since minversion targets pytest.
        {
            let required = ini_overrides.get("minversion")
                .or_else(|| ini_file.get("minversion"));
            if let Some(required) = required {
                // pytest API compatibility version (kept in sync with the
                // embedded pytest.__version__ shim).
                const PYTEST_COMPAT_VERSION: &str = "9.0.3";
                if !semver_ge(PYTEST_COMPAT_VERSION, required.trim()) {
                    let path = config_file_name.as_ref()
                        .map(|n| rootdir.join(n).display().to_string())
                        .unwrap_or_default();
                    return Err(format!(
                        "{}: 'minversion' requires pytest-{}, actual pytest-{}",
                        path, required.trim(), PYTEST_COMPAT_VERSION
                    ));
                }
            }
        }

        // pytest's -v/-q and --verbosity=N all write the same signed
        // `option.verbose`. We keep verbose (level up) and quiet_level
        // (level down) split; --verbosity=N sets the level directly.
        let (verbose, quiet_level) = match matches
            .get_one::<String>("verbosity")
            .and_then(|v| v.trim().parse::<i32>().ok())
        {
            Some(level) if level >= 0 => (level as u8, 0),
            Some(level) => (0, (-level) as u8),
            None => (matches.get_count("verbose"), matches.get_count("quiet")),
        };

        Ok(Self {
            paths: matches
                .get_many::<String>("paths")
                .map(|vals| vals.cloned().collect())
                .unwrap_or_default(),
            verbose,
            quiet: quiet_level > 0,
            quiet_level,
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
            ignored_config_files,
            effective_args,
            plugin_args,
            reporter_delegated: std::sync::atomic::AtomicBool::new(false),
        })
    }

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
    fn verbosity_for(&self, ini_name: &str) -> i32 {
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
