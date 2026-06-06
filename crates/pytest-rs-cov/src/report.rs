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

/// Display percentage, coverage.py-style (display_covered): rounded, but
/// 0% and 100% only appear when exact.
fn percent_display(covered: usize, total: usize) -> String {
    if total == 0 || covered == total {
        return "100%".to_string();
    }
    if covered == 0 {
        return "0%".to_string();
    }
    let pct = covered as f64 / total as f64 * 100.0;
    let display = if pct > 0.0 && pct < 1.0 {
        1.0
    } else if pct > 99.0 && pct < 100.0 {
        99.0
    } else {
        pct.round()
    };
    format!("{display:.0}%")
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
pub fn render_term(data: &CoverageData, missing: bool, skip_covered: bool) -> String {
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
        line.push_str(&format!("{cover:>7}"));
        if let Some(ranges) = ranges
            && !ranges.is_empty()
        {
            line.push_str(&format!("   {ranges}"));
        }
        line
    };
    let header = format!(
        "{:<name_width$}{:>8}{:>7}{}{:>7}{}",
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
    for row in &data.rows {
        let (covered_units, total_units) = row.units();
        if skip_covered && row.stmts() > 0 && covered_units == total_units {
            skipped += 1;
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
            percent_display(covered_units, total_units),
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
        percent_display(total_covered, total_units),
        None,
    ));
    out.push('\n');
    if skipped > 0 {
        out.push_str(&format!(
            "\n{skipped} file{} skipped due to complete coverage.\n",
            if skipped == 1 { "" } else { "s" }
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

/// Minimal Cobertura XML (line coverage only; branch coverage deferred).
pub fn render_xml(data: &CoverageData, rootdir: &str) -> String {
    let valid = data.total_stmts();
    let covered = valid - data.total_miss();
    let line_rate = if valid == 0 {
        1.0
    } else {
        covered as f64 / valid as f64
    };
    let mut out = String::new();
    out.push_str("<?xml version=\"1.0\" ?>\n");
    out.push_str(&format!(
        "<coverage version=\"pytest-rs-cov {}\" lines-valid=\"{valid}\" lines-covered=\"{covered}\" \
         line-rate=\"{line_rate:.4}\" branches-covered=\"0\" branches-valid=\"0\" \
         branch-rate=\"0\" complexity=\"0\">\n",
        env!("CARGO_PKG_VERSION"),
    ));
    out.push_str(&format!(
        "\t<sources>\n\t\t<source>{}</source>\n\t</sources>\n",
        xml_escape(rootdir)
    ));
    out.push_str("\t<packages>\n\t\t<package name=\".\">\n\t\t\t<classes>\n");
    for row in &data.rows {
        let rate = if row.stmts() == 0 {
            1.0
        } else {
            (row.stmts() - row.miss()) as f64 / row.stmts() as f64
        };
        out.push_str(&format!(
            "\t\t\t\t<class name=\"{0}\" filename=\"{0}\" line-rate=\"{rate:.4}\">\n\
             \t\t\t\t\t<methods/>\n\t\t\t\t\t<lines>\n",
            xml_escape(&row.name),
        ));
        for line in &row.executable {
            let hits = u32::from(row.covered.contains(line));
            out.push_str(&format!(
                "\t\t\t\t\t\t<line number=\"{line}\" hits=\"{hits}\"/>\n"
            ));
        }
        out.push_str("\t\t\t\t\t</lines>\n\t\t\t\t</class>\n");
    }
    out.push_str("\t\t\t</classes>\n\t\t</package>\n\t</packages>\n</coverage>\n");
    out
}

/// lcov tracefile format.
pub fn render_lcov(data: &CoverageData) -> String {
    let mut out = String::new();
    for row in &data.rows {
        out.push_str("TN:\n");
        out.push_str(&format!("SF:{}\n", row.name));
        for line in &row.executable {
            let hits = u32::from(row.covered.contains(line));
            out.push_str(&format!("DA:{line},{hits}\n"));
        }
        out.push_str(&format!("LF:{}\n", row.stmts()));
        out.push_str(&format!("LH:{}\n", row.stmts() - row.miss()));
        out.push_str("end_of_record\n");
    }
    out
}
