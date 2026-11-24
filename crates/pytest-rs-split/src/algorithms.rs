//! Test-splitting algorithms, ported from pytest-split 0.9.0.

use std::collections::HashMap;

pub struct TestGroup {
    /// Indices into the original item list, in original relative order.
    pub selected: Vec<usize>,
    pub duration: f64,
}

/// Per-unit durations: the known value, or the average over the known
/// units present in this run (1.0 when none is known).
fn fill_missing(known: &[Option<f64>]) -> Vec<f64> {
    let relevant: Vec<f64> = known.iter().flatten().copied().collect();
    let average = if relevant.is_empty() {
        1.0
    } else {
        relevant.iter().sum::<f64>() / relevant.len() as f64
    };
    known.iter().map(|d| d.unwrap_or(average)).collect()
}

/// Per-item durations: cached value, or the average over the cached tests
/// present in this run (1.0 when nothing is cached).
fn items_with_durations(nodeids: &[String], durations: &HashMap<String, f64>) -> Vec<f64> {
    let known: Vec<Option<f64>> = nodeids
        .iter()
        .map(|nodeid| durations.get(nodeid).copied())
        .collect();
    fill_missing(&known)
}

/// Per-file durations: the sum of the cached durations of the file's tests
/// (keys equal to the file node id or under `<file>::`); files with none
/// count as the average of the files present that have some (1.0 when
/// none does).
pub fn files_with_durations(files: &[String], durations: &HashMap<String, f64>) -> Vec<f64> {
    let mut sums: HashMap<&str, f64> = HashMap::new();
    for (nodeid, duration) in durations {
        let file = nodeid.split("::").next().unwrap_or(nodeid);
        *sums.entry(file).or_default() += duration;
    }
    let known: Vec<Option<f64>> = files
        .iter()
        .map(|file| sums.get(file.as_str()).copied())
        .collect();
    fill_missing(&known)
}

/// Contiguous chunks balanced by duration (order-preserving).
pub fn duration_based_chunks(
    splits: usize,
    nodeids: &[String],
    durations: &HashMap<String, f64>,
) -> Vec<TestGroup> {
    duration_based_chunks_of(splits, &items_with_durations(nodeids, durations))
}

/// `duration_based_chunks` over units whose durations are already resolved.
pub fn duration_based_chunks_of(splits: usize, item_durations: &[f64]) -> Vec<TestGroup> {
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

/// Greedy LPT: largest test goes to the currently-smallest group. `names`
/// (the items' reprs, as upstream sorts by `str(item)`) break ties.
pub fn least_duration(
    splits: usize,
    nodeids: &[String],
    names: &[String],
    durations: &HashMap<String, f64>,
) -> Vec<TestGroup> {
    least_duration_of(splits, names, &items_with_durations(nodeids, durations))
}

/// `least_duration` over units whose durations are already resolved;
/// `names` only break ties.
pub fn least_duration_of(
    splits: usize,
    names: &[String],
    item_durations: &[f64],
) -> Vec<TestGroup> {
    // Sort by name for cross-node determinism, then by duration
    // descending (both stable, matching upstream).
    let mut order: Vec<usize> = (0..names.len()).collect();
    order.sort_by(|&a, &b| names[a].cmp(&names[b]));
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

#[cfg(test)]
mod tests {
    use super::least_duration_of;

    #[test]
    fn ties_break_by_name_like_upstream() {
        // Upstream sorts by `str(item)` (`<Function name>`, no module), then
        // stably by duration: with equal durations the groups alternate in
        // name order, not node id order.
        let names: Vec<String> = [
            "<Function test_c>",
            "<Function test_d>",
            "<Function test_e>",
            "<Function test_a>",
        ]
        .map(String::from)
        .to_vec();
        let groups = least_duration_of(2, &names, &[1.0; 4]);
        assert_eq!(groups[0].selected, vec![1, 3]);
        assert_eq!(groups[1].selected, vec![0, 2]);
    }
}
