//! Benchmark terminal table and --benchmark-json output.

use pyo3::prelude::*;

use crate::fixture::BenchResult;

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

pub fn render_table(results: &[BenchResult], sort: &str, group_by: &str) -> String {
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
        render_group(&mut out, key.as_deref(), members, sort);
    }
    out.push_str("\nLegend:\n");
    out.push_str(
        "  Outliers: 1 Standard Deviation from Mean; \
         1.5 IQR (InterQuartile Range) from 1st Quartile and 3rd Quartile.\n",
    );
    out.push_str("  OPS: Operations Per Second, computed as 1 / Mean\n");
    out
}

fn render_group(out: &mut String, group: Option<&str>, results: &[&BenchResult], sort: &str) {
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

    // Upstream always pluralizes ("1 tests").
    let header_label = match group {
        Some(group) => format!("benchmark '{group}': {} tests", results.len()),
        None => format!("benchmark: {} tests", results.len()),
    };
    let columns = format!(
        "{:<name_width$} {:>12} {:>12} {:>12} {:>12} {:>12} {:>12} {:>10} {:>14} {:>7} {:>11}",
        format!("Name (time in {unit})"),
        "Min",
        "Max",
        "Mean",
        "StdDev",
        "Median",
        "IQR",
        "Outliers",
        "OPS (Kops/s)",
        "Rounds",
        "Iterations",
    );
    let width = columns.len();
    out.push('\n');
    out.push_str(&center(&header_label, '-', width));
    out.push('\n');
    out.push_str(&columns);
    out.push('\n');
    out.push_str(&"-".repeat(width));
    out.push('\n');
    for &index in &order {
        let result = &results[index];
        let stats = &result.stats;
        out.push_str(&format!(
            "{:<name_width$} {:>12.4} {:>12.4} {:>12.4} {:>12.4} {:>12.4} {:>12.4} {:>10} {:>14.4} {:>7} {:>11}\n",
            result.name,
            stats.min * scale,
            stats.max * scale,
            stats.mean * scale,
            stats.stddev * scale,
            stats.median * scale,
            stats.iqr * scale,
            format!("{};{}", stats.outliers.0, stats.outliers.1),
            stats.ops / 1e3,
            stats.rounds,
            stats.iterations,
        ));
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

pub fn render_json(py: Python<'_>, results: &[BenchResult]) -> PyResult<String> {
    let sys = py.import("sys")?;
    let platform = py.import("platform")?;
    let machine_info = serde_json::json!({
        "node": platform.call_method0("node")?.extract::<String>()?,
        "processor": platform.call_method0("processor")?.extract::<String>()?,
        "machine": platform.call_method0("machine")?.extract::<String>()?,
        "system": platform.call_method0("system")?.extract::<String>()?,
        "python_version": platform.call_method0("python_version")?.extract::<String>()?,
        "python_implementation":
            platform.call_method0("python_implementation")?.extract::<String>()?,
        "executable": sys.getattr("executable")?.extract::<String>()?,
    });
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
