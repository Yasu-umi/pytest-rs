//! Round statistics, matching pytest-benchmark's Stats fields.

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
pub struct Stats {
    pub min: f64,
    pub max: f64,
    pub mean: f64,
    pub stddev: f64,
    pub median: f64,
    pub q1: f64,
    pub q3: f64,
    pub iqr: f64,
    /// "mild;extreme" outlier counts (beyond 1.5/3 × IQR from the quartiles).
    pub outliers: (usize, usize),
    pub ops: f64,
    pub rounds: usize,
    pub iterations: usize,
    pub total: f64,
}

/// Quartile by linear interpolation (pytest-benchmark uses the same
/// `(len-1)*q` positional scheme via statistics.quantiles-compatible math).
fn percentile(sorted: &[f64], q: f64) -> f64 {
    if sorted.len() == 1 {
        return sorted[0];
    }
    let pos = (sorted.len() - 1) as f64 * q;
    let base = pos.floor() as usize;
    let rest = pos - base as f64;
    if base + 1 < sorted.len() {
        sorted[base] + rest * (sorted[base + 1] - sorted[base])
    } else {
        sorted[base]
    }
}

impl Stats {
    /// `times` are per-round durations; each round ran `iterations` calls.
    pub fn from_rounds(times: &[f64], iterations: usize) -> Stats {
        let mut per_iter: Vec<f64> = times.iter().map(|time| time / iterations as f64).collect();
        per_iter.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let rounds = per_iter.len();
        let total: f64 = per_iter.iter().sum();
        let mean = total / rounds as f64;
        let stddev = if rounds > 1 {
            (per_iter
                .iter()
                .map(|time| (time - mean).powi(2))
                .sum::<f64>()
                / (rounds - 1) as f64)
                .sqrt()
        } else {
            0.0
        };
        let median = percentile(&per_iter, 0.5);
        let q1 = percentile(&per_iter, 0.25);
        let q3 = percentile(&per_iter, 0.75);
        let iqr = q3 - q1;
        let mild_bounds = (q1 - 1.5 * iqr, q3 + 1.5 * iqr);
        let extreme_bounds = (q1 - 3.0 * iqr, q3 + 3.0 * iqr);
        let mild = per_iter
            .iter()
            .filter(|&&time| time < mild_bounds.0 || time > mild_bounds.1)
            .count();
        let extreme = per_iter
            .iter()
            .filter(|&&time| time < extreme_bounds.0 || time > extreme_bounds.1)
            .count();
        Stats {
            min: per_iter[0],
            max: per_iter[rounds - 1],
            mean,
            stddev,
            median,
            q1,
            q3,
            iqr,
            outliers: (mild, extreme),
            ops: if mean > 0.0 { 1.0 / mean } else { 0.0 },
            rounds,
            iterations,
            total,
        }
    }
}
