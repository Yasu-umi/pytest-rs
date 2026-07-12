//! Benchmark terminal table and --benchmark-json output.

use pyo3::prelude::*;
use pyo3::types::PyDict;

use crate::fixture::{BenchResult, py_to_json};

/// The display unit for one group table, from its fastest min.
fn unit_for_min(fastest: f64) -> (&'static str, f64) {
    if fastest < 1e-6 {
        ("ns", 1e9)
    } else if fastest < 1e-3 {
        ("us", 1e6)
    } else if fastest < 1.0 {
        ("ms", 1e3)
    } else {
        ("s", 1.0)
    }
}

/// The --benchmark-group-by label for one result; None lands in the
/// unnamed "benchmark: N tests" table. Comma-combined specs concatenate
/// their pieces with spaces (upstream's get_group_by).
fn group_key(result: &BenchResult, group_by: &str) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    for spec in group_by.split(',') {
        let piece = match spec.trim() {
            "group" => result.group.clone(),
            "name" => Some(result.name.clone()),
            "func" => Some(
                result
                    .name
                    .split('[')
                    .next()
                    .unwrap_or(&result.name)
                    .to_string(),
            ),
            "fullname" => Some(result.fullname.clone()),
            "fullfunc" => Some(
                result
                    .fullname
                    .split('[')
                    .next()
                    .unwrap_or(&result.fullname)
                    .to_string(),
            ),
            "param" => result
                .name
                .split_once('[')
                .map(|(_, rest)| rest.trim_end_matches(']').to_string()),
            spec => spec.strip_prefix("param:").and_then(|param| {
                result
                    .params
                    .iter()
                    .find(|(name, _)| name == param)
                    .map(|(name, value)| format!("{name}={value}"))
            }),
        };
        if let Some(piece) = piece {
            parts.push(piece);
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(" "))
    }
}

/// Return the machine ID string (used as the storage subdirectory name).
pub fn machine_id(py: Python<'_>) -> PyResult<String> {
    let platform = py.import("platform")?;
    let sys = py.import("sys")?;
    let system: String = platform.call_method0("system")?.extract()?;
    let impl_name: String = platform.call_method0("python_implementation")?.extract()?;
    let version_info = sys.getattr("version_info")?;
    let major: u32 = version_info.getattr("major")?.extract()?;
    let minor: u32 = version_info.getattr("minor")?.extract()?;
    let arch = platform.call_method0("architecture")?;
    let bits: String = arch.get_item(0)?.extract()?;
    let bits = bits.replace("bit", "");
    Ok(format!("{system}-{impl_name}-{major}.{minor}-{bits}bit"))
}

/// Next file number for a storage directory (scans existing 0001_*.json files).
pub fn next_num(storage_dir: &std::path::Path) -> u32 {
    let mut max = 0u32;
    if let Ok(entries) = std::fs::read_dir(storage_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.len() >= 4
                && name[..4].chars().all(|c| c.is_ascii_digit())
                && let Ok(n) = name[..4].parse::<u32>()
            {
                max = max.max(n);
            }
        }
    }
    max + 1
}

/// Find the most recent file in storage_dir matching `prefix` (if given).
pub fn find_compare_file(
    storage_dir: &std::path::Path,
    prefix: Option<&str>,
) -> Result<Option<std::path::PathBuf>, String> {
    if !storage_dir.exists() {
        return Ok(None);
    }
    let mut files: Vec<std::path::PathBuf> = std::fs::read_dir(storage_dir)
        .map_err(|e| e.to_string())?
        .flatten()
        .filter_map(|e| {
            let p = e.path();
            if p.extension().map(|ext| ext == "json").unwrap_or(false) {
                Some(p)
            } else {
                None
            }
        })
        .collect();
    files.sort();
    if let Some(prefix) = prefix {
        files.retain(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.starts_with(prefix))
                .unwrap_or(false)
        });
    }
    Ok(files.into_iter().last())
}

pub fn render_table(
    results: &[BenchResult],
    sort: &str,
    group_by: &str,
    columns: Option<&[String]>,
) -> String {
    // Partition into group tables; the unnamed group renders first, the
    // rest alphabetically (upstream's ordering).
    let mut groups: Vec<(Option<String>, Vec<&BenchResult>)> = Vec::new();
    for result in results {
        let key = group_key(result, group_by);
        match groups.iter_mut().find(|(existing, _)| *existing == key) {
            Some((_, members)) => members.push(result),
            None => groups.push((key, vec![result])),
        }
    }
    groups.sort_by(|a, b| match (&a.0, &b.0) {
        (None, None) => std::cmp::Ordering::Equal,
        (None, Some(_)) => std::cmp::Ordering::Less,
        (Some(_), None) => std::cmp::Ordering::Greater,
        (Some(x), Some(y)) => x.cmp(y),
    });

    let mut out = String::new();
    for (key, members) in &groups {
        render_group(&mut out, key.as_deref(), members, sort, columns);
    }
    out.push_str("\nLegend:\n");
    out.push_str(
        "  Outliers: 1 Standard Deviation from Mean; \
         1.5 IQR (InterQuartile Range) from 1st Quartile and 3rd Quartile.\n",
    );
    out.push_str("  OPS: Operations Per Second, computed as 1 / Mean\n");
    out
}

/// The `--benchmark-cprofile` report section (upstream's
/// `TestResults.display_cprofile`): one `ncalls`/`tottime`/`cumtime` table
/// per benchmark that captured profile rows, in original run order. Upstream
/// doesn't print a group header here either, so results aren't re-grouped.
pub fn render_cprofile(results: &[BenchResult]) -> String {
    let mut out = String::new();
    let mut section_started = false;
    for result in results {
        let Some(functions) = &result.cprofile else {
            continue;
        };
        if !section_started {
            out.push('\n');
            out.push_str(&pytest_rs_core::engine::center_with(
                "cProfile (time in s)",
                '-',
            ));
            out.push('\n');
            section_started = true;
        }
        out.push_str(&result.fullname);
        out.push('\n');
        out.push_str("ncalls\ttottime\tpercall\tcumtime\tpercall\tfilename:lineno(function)\n");
        for function in functions {
            let ncalls = function
                .get("ncalls_recursion")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            let tottime = function
                .get("tottime")
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0);
            let tottime_per = function
                .get("tottime_per")
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0);
            let cumtime = function
                .get("cumtime")
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0);
            let cumtime_per = function
                .get("cumtime_per")
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0);
            let function_name = function
                .get("function_name")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            out.push_str(&format!(
                "{ncalls}\t{tottime:.4}\t{tottime_per:.4}\t{cumtime:.4}\t{cumtime_per:.4}\t{function_name}\n"
            ));
        }
        out.push('\n');
    }
    out
}

/// One column in the benchmark result table.
struct ColSpec {
    id: &'static str,
    header: &'static str,
    width: usize,
}

const ALL_COLUMNS: &[ColSpec] = &[
    ColSpec {
        id: "min",
        header: "Min",
        width: 12,
    },
    ColSpec {
        id: "max",
        header: "Max",
        width: 12,
    },
    ColSpec {
        id: "mean",
        header: "Mean",
        width: 12,
    },
    ColSpec {
        id: "stddev",
        header: "StdDev",
        width: 12,
    },
    ColSpec {
        id: "median",
        header: "Median",
        width: 12,
    },
    ColSpec {
        id: "iqr",
        header: "IQR",
        width: 12,
    },
    ColSpec {
        id: "outliers",
        header: "Outliers",
        width: 10,
    },
    ColSpec {
        id: "ops",
        header: "OPS (Kops/s)",
        width: 14,
    },
    ColSpec {
        id: "rounds",
        header: "Rounds",
        width: 7,
    },
    ColSpec {
        id: "iterations",
        header: "Iterations",
        width: 11,
    },
];

fn col_value(id: &str, stats: &crate::stats::Stats, scale: f64) -> String {
    match id {
        "min" => format!("{:>12.4}", stats.min * scale),
        "max" => format!("{:>12.4}", stats.max * scale),
        "mean" => format!("{:>12.4}", stats.mean * scale),
        "stddev" => format!("{:>12.4}", stats.stddev * scale),
        "median" => format!("{:>12.4}", stats.median * scale),
        "iqr" => format!("{:>12.4}", stats.iqr * scale),
        "outliers" => format!(
            "{:>10}",
            format!("{};{}", stats.outliers.0, stats.outliers.1)
        ),
        "ops" => format!("{:>14.4}", stats.ops / 1e3),
        "rounds" => format!("{:>7}", stats.rounds),
        "iterations" => format!("{:>11}", stats.iterations),
        _ => String::new(),
    }
}

fn render_group(
    out: &mut String,
    group: Option<&str>,
    results: &[&BenchResult],
    sort: &str,
    columns: Option<&[String]>,
) {
    let mut order: Vec<usize> = (0..results.len()).collect();
    let key = |index: usize| -> f64 {
        let stats = &results[index].stats;
        match sort {
            "max" => stats.max,
            "mean" => stats.mean,
            "stddev" => stats.stddev,
            _ => stats.min,
        }
    };
    if sort == "name" {
        order.sort_by(|&a, &b| results[a].name.cmp(&results[b].name));
    } else if sort == "fullname" {
        order.sort_by(|&a, &b| results[a].fullname.cmp(&results[b].fullname));
    } else {
        order.sort_by(|&a, &b| {
            key(a)
                .partial_cmp(&key(b))
                .unwrap_or(std::cmp::Ordering::Equal)
        });
    }

    let fastest = results
        .iter()
        .map(|result| result.stats.min)
        .fold(f64::INFINITY, f64::min);
    let (unit, scale) = unit_for_min(fastest);
    let name_width = results
        .iter()
        .map(|result| result.name.len())
        .chain([24])
        .max()
        .unwrap_or(24);

    // Determine active columns (user-specified order, or default all).
    let active: Vec<&ColSpec> = if let Some(cols) = columns {
        cols.iter()
            .filter_map(|id| ALL_COLUMNS.iter().find(|c| c.id == id.as_str()))
            .collect()
    } else {
        ALL_COLUMNS.iter().collect()
    };

    let header_label = match group {
        Some(group) => format!("benchmark '{group}': {} tests", results.len()),
        None => format!("benchmark: {} tests", results.len()),
    };
    // Upstream reserves extra trailing space after every comparable column's
    // header (room for a " (N.NN)" best/worst scale annotation shown next to
    // each data value) except outliers/rounds/iterations, which never carry
    // one. The reservation is a flat 10 columns whenever there's more than
    // one benchmark to compare (0 when solo — nothing to compare against).
    let rpadding = if results.len() > 1 { 10 } else { 0 };
    let mut col_header = format!("{:<name_width$}", format!("Name (time in {unit})"));
    for col in &active {
        col_header.push_str(&format!(" {:>width$}", col.header, width = col.width));
        if !matches!(col.id, "outliers" | "rounds" | "iterations") {
            col_header.push_str(&" ".repeat(rpadding));
        }
    }
    let width = col_header.len();
    out.push('\n');
    out.push_str(&center(&header_label, '-', width));
    out.push('\n');
    out.push_str(&col_header);
    out.push('\n');
    out.push_str(&"-".repeat(width));
    out.push('\n');
    for &index in &order {
        let result = &results[index];
        let stats = &result.stats;
        let mut row = format!("{:<name_width$}", result.name);
        for col in &active {
            row.push(' ');
            row.push_str(&col_value(col.id, stats, scale));
        }
        row.push('\n');
        out.push_str(&row);
    }
    out.push_str(&"-".repeat(width));
    out.push('\n');
}

fn center(label: &str, fill: char, width: usize) -> String {
    let label = format!(" {label} ");
    let pad = width.saturating_sub(label.len());
    let left = pad / 2;
    format!(
        "{}{}{}",
        fill.to_string().repeat(left),
        label,
        fill.to_string().repeat(pad - left)
    )
}

/// Builds the base machine_info dict, then fires
/// `pytest_benchmark_update_machine_info(config, machine_info)` for every
/// conftest hookimpl of that name so it can mutate the dict in place
/// (upstream pytest-benchmark's extension hook). Not a formally registered
/// hookspec — any `pytest_*`-named conftest function is already collected in
/// `session.py_hooks` regardless, so no native hookspec plumbing is needed.
pub fn build_machine_info(
    py: Python<'_>,
    config: &pytest_rs_core::config::Config,
    py_hooks: &[pytest_rs_core::session::PyHook],
) -> PyResult<serde_json::Value> {
    let sys = py.import("sys")?;
    let platform = py.import("platform")?;
    let machine_info = PyDict::new(py);
    machine_info.set_item("node", platform.call_method0("node")?)?;
    machine_info.set_item("processor", platform.call_method0("processor")?)?;
    machine_info.set_item("machine", platform.call_method0("machine")?)?;
    machine_info.set_item("system", platform.call_method0("system")?)?;
    machine_info.set_item("python_version", platform.call_method0("python_version")?)?;
    machine_info.set_item(
        "python_implementation",
        platform.call_method0("python_implementation")?,
    )?;
    machine_info.set_item("executable", sys.getattr("executable")?)?;

    let mut hooks: Vec<_> = py_hooks
        .iter()
        .filter(|hook| hook.name == "pytest_benchmark_update_machine_info")
        .collect();
    hooks.sort_by_key(|h| match (h.tryfirst, h.trylast) {
        (true, _) => 0,
        (_, true) => 2,
        _ => 1,
    });
    if !hooks.is_empty() {
        let config_proxy = pytest_rs_core::python::make_py_config(py, config)?;
        for hook in hooks {
            pytest_rs_core::python::call_py_hook(
                py,
                &hook.func,
                &[
                    ("config", config_proxy.clone_ref(py)),
                    ("machine_info", machine_info.clone().into_any().unbind()),
                ],
            )?;
        }
    }
    py_to_json(py, machine_info.as_any())
}

pub fn render_json(results: &[BenchResult], machine_info: serde_json::Value) -> PyResult<String> {
    let benchmarks: Vec<serde_json::Value> = results
        .iter()
        .map(|result| {
            let stats = &result.stats;
            serde_json::json!({
                "name": result.name,
                "fullname": result.fullname,
                "group": result.group,
                "params": if result.params.is_empty() {
                    serde_json::Value::Null
                } else {
                    serde_json::Value::Object(
                        result
                            .params
                            .iter()
                            .map(|(name, value)| (name.clone(), serde_json::json!(value)))
                            .collect(),
                    )
                },
                "stats": {
                    "min": stats.min,
                    "max": stats.max,
                    "mean": stats.mean,
                    "stddev": stats.stddev,
                    "median": stats.median,
                    "iqr": stats.iqr,
                    "q1": stats.q1,
                    "q3": stats.q3,
                    "ops": stats.ops,
                    "rounds": stats.rounds,
                    "iterations": stats.iterations,
                    "total": stats.total,
                    "outliers": format!("{};{}", stats.outliers.0, stats.outliers.1),
                },
                "extra_info": result.extra_info,
            })
        })
        .collect();
    let document = serde_json::json!({
        "machine_info": machine_info,
        "version": env!("CARGO_PKG_VERSION"),
        "benchmarks": benchmarks,
    });
    serde_json::to_string_pretty(&document)
        .map_err(|e| pyo3::exceptions::PyValueError::new_err(e.to_string()))
}
