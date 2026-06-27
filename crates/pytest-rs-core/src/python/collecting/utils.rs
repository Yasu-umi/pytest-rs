#[allow(unused_imports)]
use super::super::*;
use crate::collect::{MarkData, TestItem};
use std::path::Path;

pub(crate) fn collect_error(py: Python<'_>, message: &str) -> PyErr {
    let cls = py
        .import("pytest")
        .and_then(|m| m.getattr("Collector"))
        .and_then(|c| c.getattr("CollectError"));
    match cls {
        Ok(cls) => match cls.call1((message,)) {
            Ok(instance) => PyErr::from_value(instance),
            Err(err) => err,
        },
        Err(err) => err,
    }
}

/// Some(message) when `err` is a CollectError or a Failed(pytrace=False),
/// shown without a traceback.
pub fn collect_error_message(py: Python<'_>, err: &PyErr) -> Option<String> {
    let collect_cls = py
        .import("pytest")
        .and_then(|m| m.getattr("Collector"))
        .and_then(|c| c.getattr("CollectError"))
        .ok()?;
    if err.matches(py, &collect_cls).unwrap_or(false) {
        return Some(err.value(py).to_string());
    }
    let failed_cls = py
        .import("_pytest.outcomes")
        .and_then(|m| m.getattr("Failed"))
        .ok()?;
    if err.matches(py, &failed_cls).unwrap_or(false) {
        let pytrace = err
            .value(py)
            .getattr("pytrace")
            .and_then(|v| v.extract::<bool>())
            .unwrap_or(true);
        if !pytrace {
            let msg = err
                .value(py)
                .getattr("msg")
                .and_then(|v| v.extract::<String>())
                .unwrap_or_else(|_| err.value(py).to_string());
            return Some(msg);
        }
    }
    None
}

/// pytest-style id for one parameter value.
/// The id object for one fixture param when @pytest.fixture(ids=...) was
/// given: ids[index] for a list, ids(value) for a callable. None (absent
/// ids, None entry, or error) falls back to the value-derived id.
pub(crate) fn fixture_param_id(
    py: Python<'_>,
    ids: Option<&Py<PyAny>>,
    value: &Bound<'_, PyAny>,
    index: usize,
) -> Option<Py<PyAny>> {
    let ids = ids?.bind(py);
    let id_obj = if ids.is_callable() {
        ids.call1((value,)).ok()?
    } else {
        ids.get_item(index).ok()?
    };
    if id_obj.is_none() {
        return None;
    }
    Some(id_obj.unbind())
}

/// pytest's ascii_escaped for str ids ("\x00" -> "\\x00"); printable
/// ASCII passes through untouched.
/// Upstream's _idval_from_value applied to a user-supplied id (an
/// `ids=` callable or list entry): strings ascii-escape, numbers/bools
/// stringify, anything else falls through to the default id (None).
pub(crate) fn user_id_from_value(py: Python<'_>, id: &Bound<'_, PyAny>) -> Option<String> {
    let _ = py;
    if id.is_none() {
        return None;
    }
    if let Ok(text) = id.extract::<String>() {
        return Some(ascii_escaped_str(id, text));
    }
    if id.extract::<bool>().is_ok() || id.extract::<i64>().is_ok() || id.extract::<f64>().is_ok() {
        return id.str().ok().map(|s| s.to_string());
    }
    None
}

pub(crate) fn ascii_escaped_str(value: &Bound<'_, PyAny>, s: String) -> String {
    // Pass printable ASCII through unchanged, but backslashes must be escaped
    // via unicode_escape (real pytest: "\\" → "\\\\") so node IDs are unambiguous.
    if s.chars().all(|c| matches!(c, ' '..='~')) && !s.contains('\\') {
        return s;
    }
    value
        .call_method1("encode", ("unicode_escape",))
        .and_then(|b| b.call_method1("decode", ("ascii",)))
        .and_then(|s| s.extract::<String>())
        .unwrap_or(s)
}

/// pytest's _idval: how one parametrize value renders in the test ID.
pub(crate) fn id_for_value(value: &Bound<'_, PyAny>, argname: &str, index: usize) -> String {
    if value.is_none() {
        return "None".to_string();
    }
    if let Ok(b) = value.cast::<pyo3::types::PyBool>() {
        return if b.is_true() { "True" } else { "False" }.to_string();
    }
    if let Ok(s) = value.extract::<String>() {
        return ascii_escaped_str(value, s);
    }
    // bytes: ascii_escaped = decode("ascii", "backslashreplace") with
    // non-printables escaped.
    if let Ok(bytes) = value.cast::<pyo3::types::PyBytes>() {
        return bytes
            .as_bytes()
            .iter()
            .map(|&b| {
                if matches!(b, 0x20..=0x7e) {
                    (b as char).to_string()
                } else {
                    format!("\\x{b:02x}")
                }
            })
            .collect();
    }
    // Numbers and enums all render via str() (upstream hits the number
    // branch first, so IntEnum is "30", plain Enum "Color.RED" — both str).
    let py = value.py();
    let is_enum = py
        .import("enum")
        .and_then(|m| m.getattr("Enum"))
        .and_then(|cls| value.is_instance(&cls))
        .unwrap_or(false);
    if (is_enum
        || value.cast::<pyo3::types::PyInt>().is_ok()
        || value.cast::<pyo3::types::PyFloat>().is_ok()
        || value.cast::<pyo3::types::PyComplex>().is_ok())
        && let Ok(s) = value.str()
    {
        return s.to_string();
    }
    // re.Pattern: the (escaped) pattern text.
    let is_pattern = py
        .import("re")
        .and_then(|m| m.getattr("Pattern"))
        .and_then(|cls| value.is_instance(&cls))
        .unwrap_or(false);
    if is_pattern
        && let Ok(pattern) = value.getattr("pattern")
        && let Ok(s) = pattern.extract::<String>()
    {
        return ascii_escaped_str(&pattern, s);
    }
    // Classes and functions render as their __name__.
    if let Ok(name) = value.getattr("__name__")
        && let Ok(s) = name.extract::<String>()
    {
        return s;
    }
    format!("{argname}{index}")
}

/// Read `pytestmark` from a function, class, or module. Accepts a single
/// mark or a list, and normalizes bare MarkDecorators (e.g.
/// `pytestmark = pytest.mark.asyncio`) to their Mark.
pub(crate) fn read_marks(py: Python<'_>, obj: &Bound<'_, PyAny>) -> PyResult<Vec<MarkData>> {
    // get_unpacked_marks (upstream): classes merge their whole MRO's own
    // pytestmark lists, base classes first; MarkDecorators unwrap to Marks.
    let mut marks = Vec::new();
    let get_unpacked = match py
        .import("pytest._marks")
        .and_then(|m| m.getattr("get_unpacked_marks"))
    {
        Ok(f) => f,
        Err(_) => return Ok(marks),
    };
    // Propagate TypeError (invalid pytestmark) so the caller can report it
    // as a collection error; swallow only import/getattr failures above.
    let entries = get_unpacked.call1((obj,))?;
    let Ok(iter) = entries.try_iter() else {
        return Ok(marks);
    };
    for mark in iter.flatten() {
        // Defensive: skip entries without a string name (stubs, mocks).
        let Ok(name) = mark.getattr("name").and_then(|n| n.extract::<String>()) else {
            continue;
        };
        marks.push(MarkData {
            name,
            obj: mark.unbind(),
        });
    }
    Ok(marks)
}

/// Expand `testpaths` ini globs against the rootdir (sorted per entry,
/// recursive ** supported), pytest's Config._decide_args.
pub fn glob_testpaths(py: Python<'_>, rootdir: &Path, entries: &[String]) -> PyResult<Vec<String>> {
    let glob = py.import("glob")?;
    let kwargs = pyo3::types::PyDict::new(py);
    kwargs.set_item("recursive", true)?;
    let mut out = Vec::new();
    for entry in entries {
        let pattern = rootdir.join(entry);
        let mut matches: Vec<String> = glob
            .call_method("glob", (pattern.to_string_lossy().as_ref(),), Some(&kwargs))?
            .extract()?;
        matches.sort();
        out.extend(matches);
    }
    Ok(out)
}

/// The -k matching name set for an item (upstream KeywordMatcher.from_item):
/// node-chain names (path components, class names, test name with params),
/// names assigned directly on the test function, and mark names.
pub fn keyword_match_names(py: Python<'_>, item: &TestItem) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    let (file_part, rest) = item
        .nodeid
        .split_once("::")
        .unwrap_or((item.nodeid.as_str(), ""));
    // Subdirectory and module-file names (upstream includes every chain
    // node below the root directory).
    for component in std::path::Path::new(file_part).components() {
        names.push(component.as_os_str().to_string_lossy().to_string());
    }
    for part in rest.split("::") {
        if !part.is_empty() {
            names.push(part.to_string());
        }
    }
    // Names attached to the function through direct assignment.
    if let Ok(dict) = item.func.bind(py).getattr("__dict__")
        && let Ok(keys) = dict.call_method0("keys")
        && let Ok(iter) = keys.try_iter()
    {
        for key in iter.flatten() {
            if let Ok(name) = key.extract::<String>() {
                names.push(name);
            }
        }
    }
    for mark in &item.marks {
        names.push(mark.name.clone());
    }
    // extra_keyword_matches set by pytest_pycollect_makeitem hooks on the class.
    if let Some(cls) = &item.cls
        && let Ok(extras) = cls
            .bind(py)
            .getattr("_pytest_extra_keyword_matches")
            .and_then(|v| v.extract::<Vec<String>>())
    {
        names.extend(extras);
    }
    names
}
