pub mod cache;
pub mod collect;
pub mod config;
#[cfg(feature = "xdist")]
pub mod dist;
pub mod engine;
pub mod fixture;
pub mod hooks;
#[cfg(feature = "xdist")]
pub mod ipc;
#[cfg(feature = "xdist")]
pub mod looponfail;
pub mod pastebin;
pub mod python;

pub mod report;
pub mod request;
pub mod runner;
pub mod session;
pub mod tw;
#[cfg(feature = "xdist")]
pub mod worker;

// Plugin crates must use pyo3 through this re-export so exactly one pyo3
// version exists in the dependency graph.
pub use pyo3;

/// Prefix returned by Config::from_args when --help/--version is requested.
/// The caller should print the rest of the message and exit with code 0.
pub const EXIT_ZERO_SENTINEL: &str = "\x00__exit_zero__\x00";

pub use config::{Config, OptDef, OptionParser};
pub use engine::Engine;
pub use engine::inprocess::{PluginFactory, register_plugin_factory};
pub use hooks::{HookContext, HookResult, Plugin};
pub use session::Session;

/// Env var listing native plugins to disable, comma/space-separated, matched
/// like `-p no:NAME` (bare name or `pytest_`/`pytest-` prefixed). It exists
/// because `-p no:NAME` can't reach nested `pytester` subprocess runs: the
/// pytester fixture strips `PYTEST_ADDOPTS` (upstream parity) and the outer
/// CLI's args don't propagate inward, but the process environment does. The
/// conformance harness uses it to isolate an unrelated always-on native
/// plugin out of a suite whose own nested runs assert on exact output (e.g.
/// keeping the benchmark plugin's xdist auto-disable warning out of the
/// pytest-xdist suite, which real setups never hit since they wouldn't have
/// pytest-benchmark installed while testing xdist).
pub const DISABLE_PLUGINS_ENV: &str = "PYTEST_RS_DISABLE_PLUGINS";

/// Whether a native plugin named `plugin_name` should be disabled, per either
/// a `-p no:NAME` CLI/addopts spec (in `plugin_opts`) or the
/// [`DISABLE_PLUGINS_ENV`] environment variable. Shared by the CLI
/// (`main.rs`) and in-process (`run_inprocess`) plugin-retain paths so both
/// honor the same rules.
pub fn plugin_is_disabled(plugin_name: &str, plugin_opts: &[String]) -> bool {
    fn matches(spec: &str, plugin_name: &str) -> bool {
        spec.trim()
            .trim_start_matches("pytest_")
            .trim_start_matches("pytest-")
            == plugin_name
    }
    let via_cli = plugin_opts
        .iter()
        .filter_map(|spec| spec.strip_prefix("no:"))
        .any(|disabled| matches(disabled, plugin_name));
    let via_env = std::env::var(DISABLE_PLUGINS_ENV).is_ok_and(|value| {
        value
            .split([',', ' '])
            .filter(|s| !s.is_empty())
            .any(|disabled| matches(disabled, plugin_name))
    });
    via_cli || via_env
}
