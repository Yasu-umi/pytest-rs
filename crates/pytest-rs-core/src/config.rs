use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

/// A CLI option contributed by the core or a plugin.
#[derive(Debug, Clone)]
pub struct OptDef {
    pub name: String,
    pub takes_value: bool,
    pub default: Option<String>,
    pub help: String,
}

impl OptDef {
    pub fn flag(name: &str, help: &str) -> Self {
        Self {
            name: name.trim_start_matches("--").to_string(),
            takes_value: false,
            default: None,
            help: help.to_string(),
        }
    }

    pub fn value(name: &str, default: Option<&str>, help: &str) -> Self {
        Self {
            name: name.trim_start_matches("--").to_string(),
            takes_value: true,
            default: default.map(str::to_string),
            help: help.to_string(),
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
    flags: HashSet<String>,
    values: HashMap<String, String>,
}

impl Config {
    pub fn from_args(parser: OptionParser, argv: Vec<String>) -> Result<Self, String> {
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
            );

        for opt in &parser.opts {
            let arg = clap::Arg::new(opt.name.clone())
                .long(opt.name.clone())
                .help(opt.help.clone());
            let arg = if opt.takes_value {
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
                if let Some(v) = matches.get_one::<String>(&opt.name) {
                    values.insert(opt.name.clone(), v.clone());
                }
            } else if matches.get_flag(&opt.name) {
                flags.insert(opt.name.clone());
            }
        }

        let rootdir = std::env::current_dir().map_err(|e| e.to_string())?;
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
            flags,
            values,
        })
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
}
