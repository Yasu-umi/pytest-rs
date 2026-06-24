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
    pub(crate) opts: Vec<OptDef>,
}

impl OptionParser {
    pub fn add_option(&mut self, opt: OptDef) {
        self.opts.push(opt);
    }
}
