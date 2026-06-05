//! Test-splitting algorithms, ported from pytest-split 0.9.0.

use std::collections::HashMap;

pub struct TestGroup {
    /// Indices into the original item list, in original relative order.
    pub selected: Vec<usize>,
    pub duration: f64,
}

/// Per-item durations: cached value, or the average over the cached tests
/// present in this run (1.0 when nothing is cached).
fn items_with_durations(nodeids: &[String], durations: &HashMap<String, f64>) -> Vec<f64> {
    let relevant: Vec<f64> = nodeids
        .iter()
        .filter_map(|nodeid| durations.get(nodeid).copied())
        .collect();
    let average = if relevant.is_empty() {
        1.0
    } else {
        relevant.iter().sum::<f64>() / relevant.len() as f64
    };
    nodeids
        .iter()
        .map(|nodeid| durations.get(nodeid).copied().unwrap_or(average))
        .collect()
}

/// Contiguous chunks balanced by duration (order-preserving).
pub fn duration_based_chunks(
    splits: usize,
    nodeids: &[String],
    durations: &HashMap<String, f64>,
) -> Vec<TestGroup> {
    let item_durations = items_with_durations(nodeids, durations);
    let time_per_group = item_durations.iter().sum::<f64>() / splits as f64;

    let mut groups: Vec<TestGroup> = (0..splits)
        .map(|_| TestGroup {
            selected: Vec::new(),
            duration: 0.0,
        })
        .collect();
    let mut group_idx = 0usize;
    for (index, item_duration) in item_durations.iter().enumerate() {
        if groups[group_idx].duration >= time_per_group && group_idx < splits - 1 {
            group_idx += 1;
        }
        groups[group_idx].selected.push(index);
        groups[group_idx].duration += item_duration;
    }
    groups
}

/// Greedy LPT: largest test goes to the currently-smallest group.
pub fn least_duration(
    splits: usize,
    nodeids: &[String],
    durations: &HashMap<String, f64>,
) -> Vec<TestGroup> {
    let item_durations = items_with_durations(nodeids, durations);

    // Sort by nodeid for cross-node determinism, then by duration
    // descending (both stable, matching upstream).
    let mut order: Vec<usize> = (0..nodeids.len()).collect();
    order.sort_by(|&a, &b| nodeids[a].cmp(&nodeids[b]));
    order.sort_by(|&a, &b| {
        item_durations[b]
            .partial_cmp(&item_durations[a])
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let mut groups: Vec<TestGroup> = (0..splits)
        .map(|_| TestGroup {
            selected: Vec::new(),
            duration: 0.0,
        })
        .collect();
    for &index in &order {
        // The group with the smallest (duration, group index), like the
        // upstream heap's tuple ordering.
        let group_idx = (0..splits)
            .min_by(|&a, &b| {
                groups[a]
                    .duration
                    .partial_cmp(&groups[b].duration)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then(a.cmp(&b))
            })
            .expect("splits >= 1");
        groups[group_idx].selected.push(index);
        groups[group_idx].duration += item_durations[index];
    }
    for group in &mut groups {
        // Restore original relative ordering within the group.
        group.selected.sort_unstable();
    }
    groups
}
