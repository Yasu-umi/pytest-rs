//! Benchmark terminal table and --benchmark-json output.

use pyo3::prelude::*;

use crate::fixture::BenchResult;

/// The display unit for the table, from the fastest min.
fn unit_for(results: &[BenchResult]) -> (&'static str, f64) {
    let fastest = results
        .iter()
        .map(|result| result.stats.min)
        .fold(f64::INFINITY, f64::min);
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

pub fn render_table(results: &[BenchResult], sort: &str) -> String {
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
    } else {
        order.sort_by(|&a, &b| {
            key(a)
                .partial_cmp(&key(b))
                .unwrap_or(std::cmp::Ordering::Equal)
        });
    }

    let (unit, scale) = unit_for(results);
    let name_width = results
        .iter()
        .map(|result| result.name.len())
        .chain([24])
        .max()
        .unwrap_or(24);

    let header_label = format!(
        "benchmark: {} test{}",
        results.len(),
        if results.len() == 1 { "" } else { "s" }
    );
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
    let mut out = String::new();
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
    out.push_str("\nLegend:\n");
    out.push_str(
        "  Outliers: 1 Standard Deviation from Mean; \
         1.5 IQR (InterQuartile Range) from 1st Quartile and 3rd Quartile.\n",
    );
    out.push_str("  OPS: Operations Per Second, computed as 1 / Mean\n");
    out
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
                "group": null,
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
