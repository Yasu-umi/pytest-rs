use super::options::OptionParser;
use super::types::Config;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use super::ini::{ConfigResult, find_ini, load_config_from_path, semver_ge};
use super::types::{
    common_ancestor, dirs_from_args, is_negative_number, looks_like_path, shlex_split,
};
impl Config {
    // Core pytest options parsed into flags/values (queried via
    // get_flag/get_value); some are still inert and gain behavior as
    // features land.
    const CORE_FLAGS: [&str; 34] = [
        "fixtures",             // list available fixtures and exit (a la --collect-only)
        "fixtures-per-test",    // list fixtures used by each test and exit
        "loadscope-reorder",    // xdist: reorder loadscope work units by size (default on)
        "no-loadscope-reorder", // xdist: keep collection order for loadscope work units
        "force-short-summary",  // truncate short-summary messages even at -vv
        "no-fold-skipped",      // list each skipped test in the short summary
        "xfail-tb",             // show tracebacks for xfailed tests in XFAILURES
        "no-showlocals",        // overrides an addopts --showlocals / -l
        "markers",              // list registered markers (ini + plugin-registered) and exit
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
        "nbmake",                  // accepted-but-inert: notebook collection not implemented
        "worker",                  // hidden: this process is a -n worker (IPC on stdin/stdout)
        "runxfail",                // report xfail-marked tests as if unmarked
        "setup-only",              // run fixtures, skip the tests
        "setup-plan",              // show setup/teardown plan without executing fixtures
        "setup-show",              // run tests, narrating fixture setup/teardown
        "traceconfig",             // accepted-but-inert: plugin trace header not implemented
        "keep-duplicates",         // collect the same file once per duplicated arg
        "noconftest",              // do not load any conftest.py files
        "pdb",                     // start pdb on failures
        "trace",                   // break at start of each test
        "pyargs",                  // interpret args as python module paths
        "disable-plugin-autoload", // disable loading plugins from entry points
    ];
    const CORE_VALUES: [(&str, Option<char>); 43] = [
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
        ("dist", None),               // accepted-but-inert: module-affinity load is the only mode
        ("maxprocesses", None),       // accepted-but-inert
        ("max-worker-restart", None), // accepted-but-inert: workers are not restarted
        ("tx", None),                 // xdist gateway specs ("2*popen", "popen//chdir=DIR")
        ("rsyncdir", None),           // accepted-but-inert: fork workers share the filesystem
        ("pdbcls", None),             // custom debugger class (modname:classname)
    ];

    /// Bundled-plugin names blocked via `-p no:X`/`--plugin no:X`/`-pno:X`,
    /// scanned from the raw argv (after addopts splicing, before clap parses
    /// it). A blocked plugin's own CLI options must not be registered at
    /// all, or clap accepts them instead of upstream's
    /// "unrecognized arguments" UsageError (e.g. `-p no:capture -s`: `-s`
    /// belongs to the now-disabled capture plugin). Also raises upstream's
    /// UsageError for `-p no:X` naming a conftest.py file — conftest files
    /// aren't plugins and can't be disabled this way (`--noconftest` is the
    /// real switch); upstream's `consider_pluginarg` checks this before
    /// anything else about the `-p` value.
    fn blocked_bundled_plugins(argv: &[String]) -> Result<HashSet<String>, String> {
        let mut blocked = HashSet::new();
        for (i, arg) in argv.iter().enumerate() {
            // Upstream's own pre-parse scan (Config._preparse) strips the
            // value before checking — `-p " no:capture"`-style whitespace
            // (e.g. from a single "-p no:capture" argv token) still counts.
            let value = if let Some(rest) = arg.strip_prefix("--plugin=") {
                Some(rest.trim().to_string())
            } else if arg == "-p" || arg == "--plugin" {
                argv.get(i + 1).map(|v| v.trim().to_string())
            } else {
                arg.strip_prefix("-p").map(|v| v.trim().to_string())
            };
            if let Some(name) = value.as_deref().and_then(|v| v.strip_prefix("no:")) {
                if name.ends_with("conftest.py") {
                    return Err(format!(
                        "Blocking conftest files using -p is not supported: -p no:{name}\n\
                         conftest.py files are not plugins and cannot be disabled via -p.\n"
                    ));
                }
                blocked.insert(name.to_string());
            }
        }
        Ok(blocked)
    }

    fn xdist_only(name: &str) -> bool {
        matches!(
            name,
            "numprocesses"
                | "dist"
                | "maxprocesses"
                | "worker"
                | "tx"
                | "rsyncdir"
                | "loadscope-reorder"
                | "no-loadscope-reorder"
        )
    }

    /// Assemble the clap parser: core pytest flags/values, cacheprovider
    /// and -n/xdist options (when built in), plus plugin pytest_addoption
    /// specs. Depends only on the parser's registered option specs.
    fn build_clap_command(parser: &OptionParser, blocked: &HashSet<String>) -> clap::Command {
        let mut cmd = clap::Command::new("pytest-rs")
            .disable_help_flag(false)
            .disable_version_flag(true)
            .arg(
                clap::Arg::new("version")
                    .short('V')
                    .long("version")
                    .action(clap::ArgAction::Count)
                    .help(
                        "Display pytest version and information about plugins.\n\
                         When given twice, also display information about\nplugins.",
                    ),
            )
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
        let has_xdist = cfg!(feature = "xdist");
        for flag in Self::CORE_FLAGS {
            if !has_xdist && Self::xdist_only(flag) {
                continue;
            }
            let mut arg = clap::Arg::new(flag)
                .long(flag)
                .action(clap::ArgAction::SetTrue)
                .hide(true);
            // --funcargs is upstream's deprecated alias for --fixtures.
            if flag == "fixtures" {
                arg = arg.alias("funcargs");
            }
            cmd = cmd.arg(arg);
        }
        if has_xdist {
            // xdist's `-d`: distribute with the default load scheduler.
            cmd = cmd.arg(
                clap::Arg::new("dist-load")
                    .short('d')
                    .action(clap::ArgAction::SetTrue)
                    .hide(true),
            );
            // xdist's `-f`/`--looponfail`: rerun on file changes.
            cmd = cmd.arg(
                clap::Arg::new("looponfail")
                    .short('f')
                    .long("looponfail")
                    .action(clap::ArgAction::SetTrue)
                    .hide(true),
            );
        }
        if !blocked.contains("capture") {
            cmd = cmd.arg(
                clap::Arg::new("capture-disable")
                    .short('s')
                    .action(clap::ArgAction::SetTrue)
                    .hide(true),
            );
        }
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
        for (name, short) in Self::CORE_VALUES.into_iter().chain([("rootdir-opt", None)]) {
            if !has_xdist && Self::xdist_only(name) {
                continue;
            }
            if name == "capture" && blocked.contains("capture") {
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
        cmd
    }

    /// Read the parsed clap matches into the core flag set, value map, and
    /// plugin option list (-p NAME), honoring the xdist build gate.
    fn extract_match_flags_values(
        matches: &clap::ArgMatches,
        parser: &OptionParser,
        blocked: &HashSet<String>,
    ) -> (HashSet<String>, HashMap<String, String>, Vec<String>) {
        let has_xdist = cfg!(feature = "xdist");
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
        for flag in Self::CORE_FLAGS.into_iter().chain([
            "capture-disable",
            "showlocals",
            "full-trace",
            "lf",
            "ff",
            "nf",
            "sw",
            "sw-skip",
            "sw-reset",
        ]) {
            if !has_xdist && Self::xdist_only(flag) {
                continue;
            }
            if flag == "capture-disable" && blocked.contains("capture") {
                continue;
            }
            if matches.get_flag(flag) {
                flags.insert(flag.to_string());
            }
        }
        if has_xdist && matches.get_flag("dist-load") {
            flags.insert("dist-load".to_string());
        }
        if has_xdist && matches.get_flag("looponfail") {
            flags.insert("looponfail".to_string());
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
        for (name, _) in Self::CORE_VALUES {
            if !has_xdist && Self::xdist_only(name) {
                continue;
            }
            if name == "capture" && blocked.contains("capture") {
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
        (flags, values, plugin_opts)
    }

    /// Locate the pytest config file (explicit -c, else auto-discovery) and
    /// the rootdir, validating --rootdir. Returns
    /// (rootdir, config file name, parsed ini map, ignored config files).
    fn resolve_config_and_rootdir(argv: &[String], cwd: &Path) -> Result<ConfigResult, String> {
        let cwd = cwd.to_path_buf();
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
                // Upstream resolves inipath via os.path.abspath (normalizes
                // ".." segments without following symlinks); canonicalize is
                // the closest std equivalent and the file must already exist
                // to be loaded as an ini anyway.
                let cf_path = std::fs::canonicalize(&cf_path).unwrap_or(cf_path);
                let ini_file = load_config_from_path(&cf_path)?;
                let rootdir = cf_path
                    .parent()
                    .map(|p| p.to_path_buf())
                    .unwrap_or_else(|| cwd.clone());
                let file_name = cf_path.file_name().map(|n| n.to_string_lossy().to_string());
                (rootdir, file_name, ini_file, Vec::new())
            } else {
                let ancestor = common_ancestor(&dirs_from_args(cwd.as_ref(), argv));
                let (rootdir, file_name, ini, ignored) = find_ini(&ancestor)?;
                if file_name.is_none() {
                    // No config file found anywhere. pytest's determine_setup
                    // falls back to the common ancestor of the invocation dir
                    // and the args' ancestor, so e.g. `pytest a a/b` run from
                    // the parent roots at the invocation dir, not at `a`.
                    let rootdir = common_ancestor(&[cwd.clone(), ancestor]);
                    (rootdir, file_name, ini, ignored)
                } else {
                    (rootdir, file_name, ini, ignored)
                }
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
        let rootdir = if let Some(arg) = rootdir_arg {
            // Expand $ENV_VAR references (pytest uses os.path.expandvars).
            let expanded = {
                let s = arg.clone();
                // Walk through and replace $VAR or ${VAR} tokens.
                let mut out = String::with_capacity(s.len());
                let bytes = s.as_bytes();
                let mut i = 0;
                while i < bytes.len() {
                    if bytes[i] == b'$' {
                        i += 1;
                        let braced = i < bytes.len() && bytes[i] == b'{';
                        if braced {
                            i += 1;
                        }
                        let start = i;
                        while i < bytes.len()
                            && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_')
                        {
                            i += 1;
                        }
                        if braced && i < bytes.len() && bytes[i] == b'}' {
                            i += 1;
                        }
                        let name = &s[start..i - if braced { 1 } else { 0 }];
                        if let Ok(val) = std::env::var(name) {
                            out.push_str(&val);
                        }
                    } else {
                        out.push(bytes[i] as char);
                        i += 1;
                    }
                }
                out
            };
            let path = if Path::new(&expanded).is_absolute() {
                PathBuf::from(&expanded)
            } else {
                cwd.join(&expanded)
            };
            if !path.is_dir() {
                return Err(format!(
                    "Directory '{}' not found. Check your '--rootdir' option.",
                    path.display()
                ));
            }
            // Canonicalize to resolve symlinks (macOS /var → /private/var).
            path.canonicalize().unwrap_or(path)
        } else {
            rootdir
        };
        Ok((rootdir, config_file_name, ini_file, ignored_config_files))
    }

    /// Split argv into the args clap parses (`kept`) and the long flags it
    /// doesn't know, deferred for python-plugin pytest_addoption specs.
    fn partition_plugin_args(argv: Vec<String>, cmd: &clap::Command) -> (Vec<String>, Vec<String>) {
        // Long flags clap doesn't know are deferred for python-plugin
        // option specs (pytest_addoption runs after the interpreter is up).
        // Only the self-contained `--flag` / `--flag=value` forms are
        // recognized; unregistered leftovers usage-error at configure.
        let mut known_longs: HashSet<String> = cmd
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
        // clap's auto-generated --help and --version don't appear in
        // get_arguments(), so add them explicitly so they are never
        // deferred to plugin_args (where they would cause a USAGE_ERROR).
        known_longs.insert("help".to_string());
        known_longs.insert("version".to_string());
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
            if space_value {
                // Always consume the first non-flag token: most plugin
                // options take exactly one value, and that value may be a
                // path (e.g. `--metadata-from-json-file /path/to/f.json`).
                if let Some((_, next)) = tokens.peek()
                    && (!next.starts_with('-') || is_negative_number(next))
                {
                    plugin_args.push(tokens.next().expect("peeked").1);
                }
                // Continue consuming non-flag, non-path tokens for nargs>1
                // options (e.g. `--metadata key value`).
                while let Some((_, next)) = tokens.peek() {
                    if (next.starts_with('-') && !is_negative_number(next)) || looks_like_path(next)
                    {
                        break;
                    }
                    plugin_args.push(tokens.next().expect("peeked").1);
                }
            }
        }
        (kept, plugin_args)
    }

    /// Rewrite clap's auto-generated `--help` text into upstream's
    /// argparse-based `showhelp()` shape: the section headings clap and
    /// argparse disagree on, plus pytest's own footer lines (helpconfig.py's
    /// `showhelp`) appended after the option list. The ini-options listing
    /// and "Environment variables:" section upstream also prints need a
    /// fully configured session (conftest/plugin ini keys aren't known yet
    /// at this point in argument parsing), so they aren't reproduced here.
    fn to_argparse_style_help(clap_help: &str) -> String {
        let mut out = String::new();
        for line in clap_help.lines() {
            match line {
                "Arguments:" => out.push_str("positional arguments:\n"),
                "Options:" => out.push_str("options:\n"),
                _ => {
                    out.push_str(line);
                    out.push('\n');
                }
            }
        }
        out.push('\n');
        out.push_str("to see available markers type: pytest --markers\n");
        out.push_str("to see available fixtures type: pytest --fixtures\n");
        out.push_str(
            "(shown according to specified file_or_dir or current dir if not specified; \
             fixtures with leading '_' are only shown with the '-v' option\n",
        );
        out
    }

    pub fn from_args(parser: OptionParser, argv: Vec<String>) -> Result<Self, String> {
        // `argv[0]` is always a program-name placeholder regardless of caller
        // (the real CLI's actual argv[0], or a synthetic "pytest-rs" from
        // in-process callers) — the rest is exactly what was passed to
        // `pytest.main()`/the CLI, for `config.invocation_params.args`.
        let invocation_args = argv.get(1..).map(<[String]>::to_vec).unwrap_or_default();
        let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
        let cwd = std::fs::canonicalize(&cwd).unwrap_or(cwd);
        let (rootdir, config_file_name, ini_file, ignored_config_files) =
            Self::resolve_config_and_rootdir(&argv, &cwd)?;

        // addopts from the config file are prepended to the CLI args.
        // `-o addopts=...` wins over the file: it must apply here, before
        // clap parsing, or the override could never disable addopts.
        let mut argv = argv;
        // A spawned xdist worker (spawn mode) receives the controller's
        // `effective_args`, which already include the ini addopts and
        // PYTEST_ADDOPTS expansion (PYTEST_ADDOPTS is unset on the worker so
        // the env splice below is a no-op). Re-applying the ini addopts here
        // would duplicate them and make clap reject repeated boolean flags
        // such as `--strict-markers`. Forked workers never re-enter from_args
        // (they inherit the parsed config), so `--worker` uniquely identifies
        // a spawned worker process.
        let spawned_worker = argv.iter().any(|a| a == "--worker");
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
        if !spawned_worker && let Some(addopts) = addopts {
            // A TOML-array addopts is stored NUL-joined (see parse_toml_pytest);
            // each element is already one literal argument, so split on the
            // sentinel rather than shlex-splitting (which would re-split an
            // element like "not performance"). A plain string addopts has no
            // NUL and shlex-splits: `-m "not performance"` stays one argument.
            let args: Vec<String> = if addopts.contains('\x00') {
                addopts.split('\x00').map(str::to_string).collect()
            } else {
                shlex_split(&addopts)
            };
            // Validate addopts in isolation (upstream parity): -o/-override-ini
            // must have a following value within the addopts args themselves.
            if let Some(msg) = Self::check_addopts_override_ini(&args) {
                return Err(format!(
                    "pytest: {msg}\n  config source: via addopts config"
                ));
            }
            argv.splice(1..1, args);
        }

        // Handle --version/-V before clap so we control the output format.
        // Count occurrences: single = short version, 2+ = verbose with path.
        let version_count = argv
            .iter()
            .filter(|a| *a == "--version" || *a == "-V")
            .count();
        if version_count >= 1 {
            const PYTEST_API_VERSION: &str = "9.0.3";
            let msg = if version_count >= 2 {
                // Match upstream's two-line verbose format so tests that
                // fnmatch_lines("*This is pytest version*") pass.
                format!(
                    "pytest {}\nThis is pytest version {}, imported from pytest-rs-{}\n",
                    PYTEST_API_VERSION,
                    PYTEST_API_VERSION,
                    env!("CARGO_PKG_VERSION")
                )
            } else {
                format!("pytest {}\n", PYTEST_API_VERSION)
            };
            return Err(format!("{}{}", crate::EXIT_ZERO_SENTINEL, msg));
        }

        // Expand @file arguments: each arg of the form "@path" is replaced by
        // the lines of that file (stripping \r\n, skipping blank/comment lines).
        // This matches argparse's fromfile_prefix_chars='@' behaviour.
        let argv = {
            let mut expanded: Vec<String> = Vec::with_capacity(argv.len());
            for arg in argv {
                if let Some(path) = arg.strip_prefix('@') {
                    match std::fs::read_to_string(path) {
                        Ok(contents) => {
                            for line in contents.lines() {
                                let line = line.trim_end_matches('\r');
                                if !line.is_empty() {
                                    expanded.push(line.to_string());
                                }
                            }
                        }
                        Err(err) => {
                            return Err(format!("pytest: error: {}: {}", arg, err));
                        }
                    }
                } else {
                    expanded.push(arg);
                }
            }
            expanded
        };

        let blocked = Self::blocked_bundled_plugins(&argv)?;
        let cmd = Self::build_clap_command(&parser, &blocked);

        let (argv, plugin_args) = Self::partition_plugin_args(argv, &cmd);

        let effective_args = argv.clone();
        let matches = match cmd.try_get_matches_from(argv) {
            Ok(matches) => matches,
            // --help/--version: return a sentinel so the caller can print and
            // exit cleanly. Using process::exit here would kill the Python
            // interpreter when called in-process (pytester.parseconfig()).
            Err(err)
                if matches!(
                    err.kind(),
                    clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion
                ) =>
            {
                // Use plain text (no ANSI) so downstream fnmatch_lines("*usage:*") works in
                // non-TTY subprocess contexts. Normalize clap's "Usage:" to "usage:" so
                // case-sensitive fnmatch on Linux matches upstream argparse-style patterns.
                let plain = err.render().to_string().replace("Usage:", "usage:");
                let plain = if err.kind() == clap::error::ErrorKind::DisplayHelp {
                    Self::to_argparse_style_help(&plain)
                } else {
                    plain
                };
                return Err(format!("{}{}", crate::EXIT_ZERO_SENTINEL, plain));
            }
            Err(err) => {
                // A short flag whose owning bundled plugin was just blocked
                // (`-p no:capture -s`) is unregistered, so clap rejects it
                // outright instead of deferring to apply_plugin_cli_args
                // (that path only defers unknown *long* `--flag` tokens).
                // Match pytest's argparse-style wording for this case.
                if err.kind() == clap::error::ErrorKind::UnknownArgument
                    && let Some(clap::error::ContextValue::String(arg)) =
                        err.get(clap::error::ContextKind::InvalidArg)
                {
                    return Err(format!("pytest: error: unrecognized arguments: {arg}"));
                }
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

        let (flags, values, plugin_opts) =
            Self::extract_match_flags_values(&matches, &parser, &blocked);

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
        // pytest-rs package version (0.0.5), since minversion targets pytest.
        {
            let required = ini_overrides
                .get("minversion")
                .or_else(|| ini_file.get("minversion"));
            if let Some(required) = required {
                // pytest API compatibility version (kept in sync with the
                // embedded pytest.__version__ shim).
                const PYTEST_COMPAT_VERSION: &str = "9.0.3";
                if !semver_ge(PYTEST_COMPAT_VERSION, required.trim()) {
                    let path = config_file_name
                        .as_ref()
                        .map(|n| rootdir.join(n).display().to_string())
                        .unwrap_or_default();
                    return Err(format!(
                        "{}: 'minversion' requires pytest-{}, actual pytest-{}",
                        path,
                        required.trim(),
                        PYTEST_COMPAT_VERSION
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
            invocation_args,
            reporter_delegated: std::sync::atomic::AtomicBool::new(false),
        })
    }

    /// Check addopts args for an orphaned -o/--override-ini (one with no
    /// following value within the addopts args themselves).  Returns the clap-
    /// style error message if found, or None when the args are well-formed.
    fn check_addopts_override_ini(args: &[String]) -> Option<String> {
        let mut i = 0;
        while i < args.len() {
            let arg = &args[i];
            if arg == "-o" || arg == "--override-ini" {
                let next = args.get(i + 1);
                // Missing value OR next token is another flag
                if next.is_none() || next.is_some_and(|s| s.starts_with('-')) {
                    return Some(
                        "error: argument -o/--override-ini: expected one argument".to_string(),
                    );
                }
                i += 2;
                continue;
            }
            i += 1;
        }
        None
    }
}
