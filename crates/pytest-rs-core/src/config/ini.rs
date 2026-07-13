use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub(crate) type ConfigResult = (
    PathBuf,
    Option<String>,
    HashMap<String, String>,
    Vec<String>,
    HashMap<String, String>, // toml_types: key → original TOML type tag
);

/// (stringified ini values, key → original TOML type tag)
type ParsedIni = (HashMap<String, String>, HashMap<String, String>);

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
fn load_config(dir: &Path, name: &str) -> Result<Option<ParsedIni>, String> {
    let Ok(content) = std::fs::read_to_string(dir.join(name)) else {
        return Ok(None);
    };
    let path = dir.join(name).to_string_lossy().into_owned();
    match name {
        "pytest.toml" | ".pytest.toml" => {
            let result = parse_toml_pytest(&content, Some(&path))?;
            Ok(Some(
                result.unwrap_or_else(|| (HashMap::new(), HashMap::new())),
            ))
        }
        "pytest.ini" | ".pytest.ini" => {
            if let Some(line) = detect_missing_section_header(&content) {
                return Err(format!("{}:{}: no section header defined", path, line));
            }
            Ok(Some((
                parse_ini_section(&content, "pytest").unwrap_or_default(),
                HashMap::new(),
            )))
        }
        "pyproject.toml" => parse_pyproject(&content, Some(&path)),
        "tox.ini" => Ok(Some((
            parse_ini_section(&content, "pytest").unwrap_or_default(),
            HashMap::new(),
        ))),
        "setup.cfg" => Ok(Some((
            parse_ini_section(&content, "tool:pytest").unwrap_or_default(),
            HashMap::new(),
        ))),
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
/// ignored, toml_types), where `ignored` lists lower-priority config files
/// in the same directory that also hold pytest config — pytest warns about
/// these. `toml_types` maps key → original TOML type tag for TOML sources.
///
/// Real pytest's `locate_config` fallback: if no file with pytest content is
/// found but a `pyproject.toml` exists anywhere in the walk, the closest one
/// (first encountered) becomes the inipath with an empty config dict.
/// Returns Err when a pyproject.toml conflict is detected (UsageError).
#[allow(clippy::type_complexity)]
pub(crate) fn find_ini(
    start: &Path,
) -> Result<
    (
        PathBuf,
        Option<String>,
        HashMap<String, String>,
        Vec<String>,
        HashMap<String, String>, // toml_types
    ),
    String,
> {
    let mut first_pyproject_dir: Option<PathBuf> = None;
    for dir in start.ancestors() {
        for (i, name) in CONFIG_NAMES.iter().enumerate() {
            if *name == "pyproject.toml" && first_pyproject_dir.is_none() && dir.join(name).exists()
            {
                first_pyproject_dir = Some(dir.to_path_buf());
            }
            if let Some((values, toml_types)) = load_config(dir, name)? {
                let ignored = CONFIG_NAMES[i + 1..]
                    .iter()
                    .filter(|other| load_config(dir, other).ok().flatten().is_some())
                    .map(|other| other.to_string())
                    .collect();
                return Ok((
                    dir.to_path_buf(),
                    Some(name.to_string()),
                    values,
                    ignored,
                    toml_types,
                ));
            }
        }
    }
    if let Some(dir) = first_pyproject_dir {
        return Ok((
            dir,
            Some("pyproject.toml".to_string()),
            HashMap::new(),
            Vec::new(),
            HashMap::new(),
        ));
    }
    Ok((
        start.to_path_buf(),
        None,
        HashMap::new(),
        Vec::new(),
        HashMap::new(),
    ))
}

/// Load pytest config from an explicit path (for -c/--config-file).
/// Returns Err on pyproject.toml conflicts; empty map on other failures.
/// Mirrors real pytest's `load_config_dict_from_file` dispatch rules:
/// - pytest.toml / .pytest.toml → `[pytest]` table
/// - any other .toml (including pyproject.toml and custom names) → `[tool.pytest.ini_options]`
/// - .cfg → `[tool:pytest]`
/// - .ini and everything else → `[pytest]`
pub(crate) fn load_config_from_path(path: &Path) -> Result<ParsedIni, String> {
    let Ok(content) = std::fs::read_to_string(path) else {
        return Ok((HashMap::new(), HashMap::new()));
    };
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    let path_str = path.to_string_lossy().into_owned();
    match path.extension().and_then(|e| e.to_str()) {
        Some("toml") => {
            if matches!(name, "pytest.toml" | ".pytest.toml") {
                Ok(parse_toml_pytest(&content, Some(&path_str))?
                    .unwrap_or_else(|| (HashMap::new(), HashMap::new())))
            } else {
                Ok(parse_pyproject(&content, Some(&path_str))?
                    .unwrap_or_else(|| (HashMap::new(), HashMap::new())))
            }
        }
        Some("cfg") => Ok((
            parse_ini_section(&content, "tool:pytest").unwrap_or_default(),
            HashMap::new(),
        )),
        _ => Ok((
            parse_ini_section(&content, "pytest").unwrap_or_default(),
            HashMap::new(),
        )),
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
pub(crate) fn semver_ge(current: &str, required: &str) -> bool {
    let parse = |v: &str| -> (u64, u64, u64) {
        let mut parts = v.split('.');
        let major = parts
            .next()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0);
        let minor = parts
            .next()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0);
        let patch = parts
            .next()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0);
        (major, minor, patch)
    };
    parse(current) >= parse(required)
}

/// pytest config from pyproject.toml: [tool.pytest] (pytest 9 toml mode,
/// keys other than ini_options) or [tool.pytest.ini_options] (ini mode).
/// Returns Err when both styles are present simultaneously (upstream
/// UsageError), Ok(None) when no pytest config is found.
fn parse_pyproject(content: &str, path: Option<&str>) -> Result<Option<ParsedIni>, String> {
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
    if has_native {
        return Ok(Some(render_toml_entries(tool_pytest.iter().collect())));
    }
    // [tool.pytest.ini_options]: upstream's INI mode — scalars stringify and
    // types never get validated, unlike the native [tool.pytest] TOML mode
    // above. render_toml_entries's stringified `values` map is exactly the
    // INI-mode form; discard its toml_types (INI mode carries none).
    match tool_pytest.get("ini_options").and_then(|v| v.as_table()) {
        Some(t) => Ok(Some((
            render_toml_entries(t.iter().collect()).0,
            HashMap::new(),
        ))),
        None => Ok(None),
    }
}

/// pytest config from a standalone pytest.toml / .pytest.toml: a top-level
/// `[pytest]` table (pytest 9 toml mode). Returns None when no `[pytest]`
/// table is present (the caller still treats the file as config, just empty).
fn parse_toml_pytest(content: &str, path: Option<&str>) -> Result<Option<ParsedIni>, String> {
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

/// The TOML type tag for a scalar value ("string"/"int"/"float"/"bool"),
/// used both for top-level values and for individual array items.
fn toml_scalar_type_tag(value: &toml::Value) -> &'static str {
    match value {
        toml::Value::String(_) => "string",
        toml::Value::Integer(_) => "int",
        toml::Value::Float(_) => "float",
        toml::Value::Boolean(_) => "bool",
        toml::Value::Array(_) | toml::Value::Table(_) | toml::Value::Datetime(_) => "string",
    }
}

/// Render TOML pytest entries into the engine's stringified ini form: scalar
/// values become their string, arrays become NUL-joined linelists.
/// Also returns a map of key → original TOML type tag for type validation.
/// An array's tag is `"array:<item_type_0>\x00<item_type_1>..."` so
/// `_coerce_ini` can report which index/type broke a strings-only list.
fn render_toml_entries(entries: Vec<(&String, &toml::Value)>) -> ParsedIni {
    let mut values = HashMap::new();
    let mut toml_types = HashMap::new();
    for (key, value) in entries {
        let (rendered, type_tag) = match value {
            toml::Value::String(s) => (s.clone(), "string".to_string()),
            toml::Value::Array(items) => {
                let rendered_items: Vec<String> = items
                    .iter()
                    .map(|item| match item {
                        toml::Value::String(s) => s.clone(),
                        other => other.to_string(),
                    })
                    .collect();
                let item_types: Vec<&str> = items.iter().map(toml_scalar_type_tag).collect();
                (
                    // NUL-byte sentinel: signals to _coerce_ini that this is a
                    // pre-split TOML array (items may contain spaces), not an
                    // ini-file string that needs shlex.split().
                    rendered_items.join("\x00"),
                    format!("array:{}", item_types.join("\x00")),
                )
            }
            toml::Value::Integer(_) => (value.to_string(), "int".to_string()),
            toml::Value::Float(_) => (value.to_string(), "float".to_string()),
            toml::Value::Boolean(_) => (value.to_string(), "bool".to_string()),
            toml::Value::Table(_) | toml::Value::Datetime(_) => {
                (value.to_string(), "string".to_string())
            }
        };
        values.insert(key.clone(), rendered);
        toml_types.insert(key.clone(), type_tag);
    }
    (values, toml_types)
}
