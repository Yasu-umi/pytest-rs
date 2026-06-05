//! Coverage report rendering: terminal table, Cobertura XML, lcov.

use std::collections::BTreeSet;

pub struct FileRow {
    /// Path relative to rootdir (display name and report key).
    pub name: String,
    pub executable: BTreeSet<u32>,
    pub covered: BTreeSet<u32>,
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
}

pub struct CoverageData {
    pub rows: Vec<FileRow>,
}

impl CoverageData {
    pub fn total_stmts(&self) -> usize {
        self.rows.iter().map(FileRow::stmts).sum()
    }

    pub fn total_miss(&self) -> usize {
        self.rows.iter().map(FileRow::miss).sum()
    }

    /// Total coverage percentage (exact, for --cov-fail-under).
    pub fn total_percent(&self) -> f64 {
        percent_exact(self.total_stmts(), self.total_miss())
    }
}

/// Display percentage, coverage.py-style (display_covered): rounded, but
/// 0% and 100% only appear when exact.
fn percent_display(stmts: usize, miss: usize) -> String {
    if stmts == 0 || miss == 0 {
        return "100%".to_string();
    }
    let pct = (stmts - miss) as f64 / stmts as f64 * 100.0;
    let display = if pct > 0.0 && pct < 1.0 {
        1.0
    } else if pct > 99.0 && pct < 100.0 {
        99.0
    } else {
        pct.round()
    };
    format!("{display:.0}%")
}

fn percent_exact(stmts: usize, miss: usize) -> f64 {
    if stmts == 0 {
        100.0
    } else {
        (stmts - miss) as f64 / stmts as f64 * 100.0
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

/// The pytest-cov terminal section (term / term-missing).
pub fn render_term(
    data: &CoverageData,
    missing: bool,
    skip_covered: bool,
    python_version: &str,
) -> String {
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
    out.push('\n');

    let name_width = data
        .rows
        .iter()
        .map(|row| row.name.len())
        .chain(["Name".len(), "TOTAL".len()])
        .max()
        .unwrap_or(4);
    let render_row = |name: &str, stmts: usize, miss: usize, ranges: Option<String>| {
        let mut line = format!(
            "{:<name_width$}{:>8}{:>7}{:>7}",
            name,
            stmts,
            miss,
            percent_display(stmts, miss),
        );
        if let Some(ranges) = ranges
            && !ranges.is_empty()
        {
            line.push_str(&format!("   {ranges}"));
        }
        line
    };
    let header = format!(
        "{:<name_width$}{:>8}{:>7}{:>7}{}",
        "Name",
        "Stmts",
        "Miss",
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
        if skip_covered && row.stmts() > 0 && row.miss() == 0 {
            skipped += 1;
            continue;
        }
        let ranges = missing.then(|| missing_ranges(&row.missing_lines()));
        out.push_str(&render_row(&row.name, row.stmts(), row.miss(), ranges));
        out.push('\n');
    }
    out.push_str(&rule);
    out.push('\n');
    out.push_str(&render_row(
        "TOTAL",
        data.total_stmts(),
        data.total_miss(),
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
