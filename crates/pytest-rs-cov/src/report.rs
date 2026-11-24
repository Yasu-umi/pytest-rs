//! Coverage report rendering: terminal table, Cobertura XML, lcov.

use std::collections::{BTreeMap, BTreeSet};

use crate::analysis::EXIT;

pub struct FileRow {
    /// Path relative to rootdir (display name and report key).
    pub name: String,
    pub executable: BTreeSet<u32>,
    pub covered: BTreeSet<u32>,
    /// Branch mode: possible destinations per branch line ([body, onward]).
    pub branches: BTreeMap<u32, Vec<i64>>,
    /// Branch mode: destinations actually taken per branch line.
    pub taken: BTreeMap<u32, BTreeSet<i64>>,
}

impl FileRow {
    pub fn stmts(&self) -> usize {
        self.executable.len()
    }

    pub fn miss(&self) -> usize {
        self.executable.difference(&self.covered).count()
    }

    fn missing_lines(&self) -> Vec<u32> {
        self.executable.difference(&self.covered).copied().collect()
    }

    /// Total possible branch destinations.
    pub fn n_branches(&self) -> usize {
        self.branches.values().map(Vec::len).sum()
    }

    /// Destinations exercised.
    pub fn n_taken(&self) -> usize {
        self.taken.values().map(BTreeSet::len).sum()
    }

    fn taken_at(&self, line: u32) -> usize {
        self.taken.get(&line).map(BTreeSet::len).unwrap_or(0)
    }

    /// Branch lines that executed but did not exercise every destination
    /// (coverage.py's BrPart).
    pub fn n_partial(&self) -> usize {
        self.branches
            .iter()
            .filter(|(line, dests)| {
                self.covered.contains(line) && self.taken_at(**line) < dests.len()
            })
            .count()
    }

    /// Coverage units: lines plus branch destinations (coverage.py's
    /// percent definition in branch mode).
    fn units(&self) -> (usize, usize) {
        let covered = self.covered.len() + self.n_taken();
        let total = self.stmts() + self.n_branches();
        (covered, total)
    }
}

pub struct CoverageData {
    pub rows: Vec<FileRow>,
    /// Branch coverage was measured (adds Branch/BrPart columns).
    pub branch: bool,
    /// `[report] skip_empty`: files without statements are left out.
    pub skip_empty: bool,
    /// `[report] sort` (already validated by [`ReportSort::parse`]).
    pub sort: ReportSort,
}

/// `[report] sort`: the terminal table's row order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SortColumn {
    #[default]
    Name,
    Stmts,
    Miss,
    Branch,
    BrPart,
    Cover,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ReportSort {
    pub column: SortColumn,
    pub reverse: bool,
}

impl ReportSort {
    /// coverage.py's `sort` option: a column name, `-` to reverse, `+`
    /// allowed; Branch / BrPart only in branch mode. The error is
    /// coverage.py's message.
    pub fn parse(value: Option<&str>, branch: bool) -> Result<Self, String> {
        let Some(raw) = value else {
            return Ok(Self::default());
        };
        let lowered = raw.to_lowercase();
        let (reverse, name) = match lowered.strip_prefix('-') {
            Some(rest) => (true, rest),
            None => (false, lowered.strip_prefix('+').unwrap_or(&lowered)),
        };
        let column = match name {
            "name" => SortColumn::Name,
            "stmts" => SortColumn::Stmts,
            "miss" => SortColumn::Miss,
            "cover" => SortColumn::Cover,
            "branch" if branch => SortColumn::Branch,
            "brpart" if branch => SortColumn::BrPart,
            _ => return Err(format!("Invalid sorting option: '{raw}'")),
        };
        Ok(Self { column, reverse })
    }
}

/// coverage.py's `human_key`: digit runs compare as numbers.
fn human_key(text: &str) -> Vec<(u8, u128, String)> {
    let mut out = Vec::new();
    let mut rest = text;
    while !rest.is_empty() {
        let digits = rest.len() - rest.trim_start_matches(|c: char| c.is_ascii_digit()).len();
        if digits > 0 {
            let (number, tail) = rest.split_at(digits);
            out.push((1, number.parse().unwrap_or(u128::MAX), String::new()));
            rest = tail;
        } else {
            let end = rest
                .find(|c: char| c.is_ascii_digit())
                .unwrap_or(rest.len());
            let (chunk, tail) = rest.split_at(end);
            out.push((0, 0, chunk.to_string()));
            rest = tail;
        }
    }
    out
}

impl CoverageData {
    pub fn total_stmts(&self) -> usize {
        self.rows.iter().map(FileRow::stmts).sum()
    }

    pub fn total_miss(&self) -> usize {
        self.rows.iter().map(FileRow::miss).sum()
    }

    pub fn total_branches(&self) -> usize {
        self.rows.iter().map(FileRow::n_branches).sum()
    }

    pub fn total_partial(&self) -> usize {
        self.rows.iter().map(FileRow::n_partial).sum()
    }

    /// Total coverage percentage (exact, for --cov-fail-under).
    pub fn total_percent(&self) -> f64 {
        let covered: usize = self.rows.iter().map(|r| r.units().0).sum();
        let total: usize = self.rows.iter().map(|r| r.units().1).sum();
        percent_exact_units(covered, total)
    }
}

/// Display percentage, coverage.py-style (display_covered): rounded to
/// `precision` decimals, but 0% and 100% only appear when exact.
fn percent_display(covered: usize, total: usize, precision: usize) -> String {
    if total == 0 || covered == total {
        return format!("{:.precision$}%", 100.0_f64);
    }
    if covered == 0 {
        return format!("{:.precision$}%", 0.0_f64);
    }
    let pct = covered as f64 / total as f64 * 100.0;
    let unit = 10.0_f64.powi(-(precision as i32));
    let display = if pct > 0.0 && pct < unit {
        unit
    } else if pct > 100.0 - unit && pct < 100.0 {
        100.0 - unit
    } else {
        (pct / unit).round() * unit
    };
    format!("{display:.precision$}%")
}

fn percent_exact_units(covered: usize, total: usize) -> f64 {
    if total == 0 {
        100.0
    } else {
        covered as f64 / total as f64 * 100.0
    }
}

/// "3, 7-9" style missing-line ranges.
fn missing_ranges(lines: &[u32]) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut iter = lines.iter().copied().peekable();
    while let Some(start) = iter.next() {
        let mut end = start;
        while iter.peek() == Some(&(end + 1)) {
            end = iter.next().expect("peeked");
        }
        parts.push(if start == end {
            start.to_string()
        } else {
            format!("{start}-{end}")
        });
    }
    parts.join(", ")
}

/// Missing lines plus partial-branch arc annotations, coverage.py-style
/// ("11, 13->15, 20->exit"), ordered by line. Arcs into missing lines are
/// omitted (the missing line already says it).
fn missing_with_arcs(row: &FileRow) -> String {
    let missing: BTreeSet<u32> = row.missing_lines().into_iter().collect();
    let mut items: Vec<(u32, String)> = Vec::new();
    let lines: Vec<u32> = missing.iter().copied().collect();
    let mut iter = lines.iter().copied().peekable();
    while let Some(start) = iter.next() {
        let mut end = start;
        while iter.peek() == Some(&(end + 1)) {
            end = iter.next().expect("peeked");
        }
        items.push((
            start,
            if start == end {
                start.to_string()
            } else {
                format!("{start}-{end}")
            },
        ));
    }
    for (line, dests) in &row.branches {
        if !row.covered.contains(line) {
            continue;
        }
        for dest in dests {
            let taken = row
                .taken
                .get(line)
                .is_some_and(|taken| taken.contains(dest));
            if taken {
                continue;
            }
            if *dest > 0 && missing.contains(&(*dest as u32)) {
                continue;
            }
            let dest_text = if *dest == EXIT {
                "exit".to_string()
            } else {
                dest.to_string()
            };
            items.push((*line, format!("{line}->{dest_text}")));
        }
    }
    items.sort();
    items
        .into_iter()
        .map(|(_, text)| text)
        .collect::<Vec<_>>()
        .join(", ")
}

/// The "tests coverage" section header (printed whenever any coverage
/// report was requested, table or not — pytest-cov does the same).
pub fn render_header(python_version: &str) -> String {
    let mut out = String::new();
    out.push_str(&pytest_rs_core::engine::center_with("tests coverage", '='));
    out.push('\n');
    out.push_str(&pytest_rs_core::engine::center_with(
        &format!(
            "coverage: platform {}, python {}",
            std::env::consts::OS,
            python_version,
        ),
        '_',
    ));
    out.push('\n');
    out
}

/// The pytest-cov terminal table (term / term-missing), header excluded.
pub fn render_term(
    data: &CoverageData,
    missing: bool,
    skip_covered: bool,
    precision: usize,
) -> String {
    let mut out = String::new();
    out.push('\n');

    let name_width = data
        .rows
        .iter()
        .map(|row| row.name.len())
        .chain(["Name".len(), "TOTAL".len()])
        .max()
        .unwrap_or(4);
    let branch = data.branch;
    // The Cover column grows with --cov-precision ("88.888889%").
    let cover_width = data
        .rows
        .iter()
        .map(|row| {
            let (covered_units, total_units) = row.units();
            percent_display(covered_units, total_units, precision).len()
        })
        .chain(["Cover".len()])
        .max()
        .unwrap_or(5)
        + 2;
    let render_row = |name: &str,
                      stmts: usize,
                      miss: usize,
                      branches: usize,
                      partial: usize,
                      cover: String,
                      ranges: Option<String>| {
        let mut line = format!("{name:<name_width$}{stmts:>8}{miss:>7}");
        if branch {
            line.push_str(&format!("{branches:>9}{partial:>9}"));
        }
        line.push_str(&format!("{cover:>cover_width$}"));
        if let Some(ranges) = ranges
            && !ranges.is_empty()
        {
            line.push_str(&format!("   {ranges}"));
        }
        line
    };
    let header = format!(
        "{:<name_width$}{:>8}{:>7}{}{:>cover_width$}{}",
        "Name",
        "Stmts",
        "Miss",
        if branch {
            format!("{:>9}{:>9}", "Branch", "BrPart")
        } else {
            String::new()
        },
        "Cover",
        if missing { "   Missing" } else { "" },
    );
    let rule = "-".repeat(header.len());
    out.push_str(&header);
    out.push('\n');
    out.push_str(&rule);
    out.push('\n');
    let mut skipped = 0usize;
    let mut empty = 0usize;
    let mut ordered: Vec<&FileRow> = data.rows.iter().collect();
    let percent = |row: &FileRow| {
        let (covered, total) = row.units();
        if total == 0 {
            100.0
        } else {
            covered as f64 * 100.0 / total as f64
        }
    };
    match data.sort.column {
        SortColumn::Name => ordered.sort_by_key(|row| human_key(&row.name)),
        column => ordered.sort_by(|a, b| {
            let value = |row: &FileRow| match column {
                SortColumn::Stmts => row.stmts() as f64,
                SortColumn::Miss => row.miss() as f64,
                SortColumn::Branch => row.n_branches() as f64,
                SortColumn::BrPart => row.n_partial() as f64,
                _ => percent(row),
            };
            value(a)
                .total_cmp(&value(b))
                .then_with(|| a.name.cmp(&b.name))
        }),
    }
    if data.sort.reverse {
        ordered.reverse();
    }
    for row in ordered {
        let (covered_units, total_units) = row.units();
        if skip_covered && row.stmts() > 0 && covered_units == total_units {
            skipped += 1;
            continue;
        }
        if data.skip_empty && row.stmts() == 0 {
            empty += 1;
            continue;
        }
        let ranges = missing.then(|| {
            if branch {
                missing_with_arcs(row)
            } else {
                missing_ranges(&row.missing_lines())
            }
        });
        out.push_str(&render_row(
            &row.name,
            row.stmts(),
            row.miss(),
            row.n_branches(),
            row.n_partial(),
            percent_display(covered_units, total_units, precision),
            ranges,
        ));
        out.push('\n');
    }
    out.push_str(&rule);
    out.push('\n');
    let total_covered: usize = data.rows.iter().map(|r| r.units().0).sum();
    let total_units: usize = data.rows.iter().map(|r| r.units().1).sum();
    out.push_str(&render_row(
        "TOTAL",
        data.total_stmts(),
        data.total_miss(),
        data.total_branches(),
        data.total_partial(),
        percent_display(total_covered, total_units, precision),
        None,
    ));
    out.push('\n');
    if skipped > 0 {
        out.push_str(&format!(
            "\n{skipped} file{} skipped due to complete coverage.\n",
            if skipped == 1 { "" } else { "s" }
        ));
    }
    if empty > 0 {
        out.push_str(&format!(
            "\n{empty} empty file{} skipped.\n",
            if empty == 1 { "" } else { "s" }
        ));
    }
    out
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// coverage.py's `rate`: "1" for nothing to cover or everything covered,
/// "0" for nothing covered, otherwise four significant digits that never
/// round to 0 or 1.
fn rate(hit: usize, num: usize) -> String {
    if num == 0 || hit == num {
        "1".to_string()
    } else if hit == 0 {
        "0".to_string()
    } else {
        // Python's `f"{frac:.4g}"` for 0.0001 <= frac < 1: four significant
        // digits, trailing zeros dropped.
        let frac = (hit as f64 / num as f64).clamp(0.0001, 0.9999);
        let exponent = frac.log10().floor() as i32;
        let decimals = (3 - exponent).max(0) as usize;
        let text = format!("{frac:.decimals$}");
        text.trim_end_matches('0').trim_end_matches('.').to_string()
    }
}

impl FileRow {
    /// Branch line -> (destinations, taken), coverage.py's `branch_stats`.
    fn branch_stats(&self) -> BTreeMap<u32, (usize, usize)> {
        self.branches
            .iter()
            .map(|(line, dests)| {
                let taken = if self.covered.contains(line) {
                    self.taken_at(*line)
                } else {
                    0
                };
                (*line, (dests.len(), taken))
            })
            .collect()
    }

    /// The destinations of a branch line not taken ("exit" for leaving).
    fn missing_dests(&self, line: u32) -> Vec<String> {
        let taken = self.taken.get(&line);
        self.branches
            .get(&line)
            .into_iter()
            .flatten()
            .filter(|dest| !taken.is_some_and(|t| t.contains(dest)))
            .map(|dest| {
                if *dest == EXIT {
                    "exit".to_string()
                } else {
                    dest.to_string()
                }
            })
            .collect()
    }
}

/// Cobertura XML, for when the coverage package is not installed (it
/// renders the report itself otherwise). One package, file names relative
/// to the rootdir.
pub fn render_xml(data: &CoverageData, rootdir: &str) -> String {
    let rows: Vec<&FileRow> = data
        .rows
        .iter()
        .filter(|row| !(data.skip_empty && row.stmts() == 0))
        .collect();
    let valid: usize = rows.iter().map(|row| row.stmts()).sum();
    let covered: usize = rows.iter().map(|row| row.stmts() - row.miss()).sum();
    let (br_valid, br_covered) = if data.branch {
        rows.iter()
            .flat_map(|row| row.branch_stats().into_values())
            .fold((0, 0), |(v, c), (total, taken)| (v + total, c + taken))
    } else {
        (0, 0)
    };
    let branch_rate = |hit, num| {
        if data.branch {
            rate(hit, num)
        } else {
            "0".to_string()
        }
    };
    let mut out = String::new();
    out.push_str("<?xml version=\"1.0\" ?>\n");
    out.push_str(&format!(
        "<coverage version=\"pytest-rs-cov {}\" lines-valid=\"{valid}\" lines-covered=\"{covered}\" \
         line-rate=\"{}\" branches-covered=\"{br_covered}\" branches-valid=\"{br_valid}\" \
         branch-rate=\"{}\" complexity=\"0\">\n",
        env!("CARGO_PKG_VERSION"),
        rate(covered, valid),
        branch_rate(br_covered, br_valid),
    ));
    out.push_str(&format!(
        "\t<sources>\n\t\t<source>{}</source>\n\t</sources>\n",
        xml_escape(rootdir)
    ));
    out.push_str(&format!(
        "\t<packages>\n\t\t<package name=\".\" line-rate=\"{}\" branch-rate=\"{}\" complexity=\"0\">\n\t\t\t<classes>\n",
        rate(covered, valid),
        branch_rate(br_covered, br_valid),
    ));
    for row in rows {
        let stats = row.branch_stats();
        let (total, taken) = stats.values().fold((0, 0), |(v, c), (t, k)| (v + t, c + k));
        out.push_str(&format!(
            "\t\t\t\t<class name=\"{0}\" filename=\"{0}\" complexity=\"0\" line-rate=\"{1}\" branch-rate=\"{2}\">\n\
             \t\t\t\t\t<methods/>\n\t\t\t\t\t<lines>\n",
            xml_escape(&row.name),
            rate(row.stmts() - row.miss(), row.stmts()),
            branch_rate(taken, total),
        ));
        for line in &row.executable {
            let hits = u32::from(row.covered.contains(line));
            let mut attrs = format!("number=\"{line}\" hits=\"{hits}\"");
            if data.branch
                && let Some((total, taken)) = stats.get(line)
            {
                attrs.push_str(&format!(
                    " branch=\"true\" condition-coverage=\"{}% ({taken}/{total})\"",
                    100 * taken / total
                ));
                let missing = row.missing_dests(*line);
                if !missing.is_empty() {
                    attrs.push_str(&format!(" missing-branches=\"{}\"", missing.join(",")));
                }
            }
            out.push_str(&format!("\t\t\t\t\t\t<line {attrs}/>\n"));
        }
        out.push_str("\t\t\t\t\t</lines>\n\t\t\t\t</class>\n");
    }
    out.push_str("\t\t\t</classes>\n\t\t</package>\n\t</packages>\n</coverage>\n");
    out
}

/// lcov tracefile, for when the coverage package is not installed. Branch
/// records name a destination by its line ("exit" for leaving the code
/// object); coverage.py's own report describes them in words and adds
/// function records.
pub fn render_lcov(data: &CoverageData) -> String {
    let mut out = String::new();
    for row in data
        .rows
        .iter()
        .filter(|row| !(data.skip_empty && row.stmts() == 0))
    {
        out.push_str(&format!("SF:{}\n", row.name));
        for line in &row.executable {
            let hits = u32::from(row.covered.contains(line));
            out.push_str(&format!("DA:{line},{hits}\n"));
        }
        if row.stmts() > 0 {
            out.push_str(&format!("LF:{}\n", row.stmts()));
            out.push_str(&format!("LH:{}\n", row.stmts() - row.miss()));
        }
        if data.branch {
            let stats = row.branch_stats();
            for (line, (_, taken)) in &stats {
                let mut dests: Vec<i64> = row.branches[line].clone();
                // Exits after the other destinations, like coverage.py.
                dests.sort_by_key(|dest| (*dest < 0, *dest));
                for dest in dests {
                    let name = if dest == EXIT {
                        "exit".to_string()
                    } else {
                        format!("line {dest}")
                    };
                    let hit = if *taken == 0 {
                        "-"
                    } else if row.taken.get(line).is_some_and(|t| t.contains(&dest)) {
                        "1"
                    } else {
                        "0"
                    };
                    out.push_str(&format!("BRDA:{line},0,{name},{hit}\n"));
                }
            }
            let (brf, brh) = stats.values().fold((0, 0), |(v, c), (t, k)| (v + t, c + k));
            if brf > 0 {
                out.push_str(&format!("BRF:{brf}\nBRH:{brh}\n"));
            }
        }
        out.push_str("end_of_record\n");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::rate;

    #[test]
    fn rate_matches_coverage_py() {
        assert_eq!(rate(0, 0), "1");
        assert_eq!(rate(3, 3), "1");
        assert_eq!(rate(0, 3), "0");
        assert_eq!(rate(1, 3), "0.3333");
        assert_eq!(rate(1, 2), "0.5");
        assert_eq!(rate(1, 30), "0.03333");
        assert_eq!(rate(1, 100000), "0.0001");
        assert_eq!(rate(99999, 100000), "0.9999");
    }
}
