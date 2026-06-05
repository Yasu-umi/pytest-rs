use pytest_rs_core::pyo3::Python;
use pytest_rs_core::{Config, Engine, OptionParser, Plugin};

// Feature-gated pushes; vec![] cannot hold cfg'd elements.
#[allow(clippy::vec_init_then_push)]
fn build_plugins() -> Vec<Box<dyn Plugin>> {
    let mut plugins: Vec<Box<dyn Plugin>> = Vec::new();
    #[cfg(feature = "asyncio")]
    plugins.push(Box::new(pytest_rs_asyncio::AsyncioPlugin::new()));
    #[cfg(feature = "mock")]
    plugins.push(Box::new(pytest_rs_mock::MockPlugin::new()));
    plugins
}

fn main() {
    let plugins = build_plugins();

    let mut parser = OptionParser::default();
    for plugin in &plugins {
        plugin.pytest_addoption(&mut parser);
    }

    let argv: Vec<String> = std::env::args().collect();
    let config = match Config::from_args(parser, argv) {
        Ok(config) => config,
        Err(message) => {
            eprintln!("{message}");
            std::process::exit(pytest_rs_core::report::exit_code::USAGE_ERROR);
        }
    };

    let mut engine = Engine::new(plugins, config);
    let code = Python::attach(|py| engine.run(py));
    std::process::exit(code);
}
