//! Executable-line analysis: which lines of a source file can produce
//! sys.monitoring LINE events. This is the coverage denominator, the
//! counterpart of coverage.py's parser (simplified; documented deltas are
//! acceptable in v1).

use std::collections::BTreeSet;

use ruff_python_ast::{ElifElseClause, ExceptHandler, Expr, Stmt};
use ruff_source_file::LineIndex;
use ruff_text_size::{Ranged, TextSize};

/// A statement span used for `# pragma: no cover` block exclusion.
struct Span {
    header: u32,
    start: u32,
    end: u32,
}

struct Walker<'a> {
    index: &'a LineIndex,
    lines: BTreeSet<u32>,
    spans: Vec<Span>,
}

pub struct FileAnalysis {
    pub executable: BTreeSet<u32>,
    /// Lines excluded via `# pragma: no cover` (kept so observed hits on
    /// excluded lines stay out of the denominator).
    pub excluded: BTreeSet<u32>,
}

/// coverage.py's default exclude_lines regex (`# pragma: no cover`).
pub const DEFAULT_EXCLUDE: &str = r"#\s*(pragma|PRAGMA)[:\s]?\s*(no|NO)\s*(cover|COVER)";

/// The executable lines of `source`, or None if it does not parse.
/// `excludes` are the effective exclude_lines regexes: a match on a
/// statement's header line excludes the whole statement.
pub fn analyze(source: &str, excludes: &[regex::Regex]) -> Option<FileAnalysis> {
    let parsed = ruff_python_parser::parse_module(source).ok()?;
    let index = LineIndex::from_source_text(source);
    let mut walker = Walker {
        index: &index,
        lines: BTreeSet::new(),
        spans: Vec::new(),
    };
    walker.visit_body(&parsed.syntax().body);

    let mut excluded: BTreeSet<u32> = BTreeSet::new();
    for (lineno, line) in source.lines().enumerate() {
        if excludes.iter().any(|pattern| pattern.is_match(line)) {
            let line_number = (lineno + 1) as u32;
            excluded.insert(line_number);
            for span in &walker.spans {
                if span.header == line_number {
                    excluded.extend(span.start..=span.end);
                }
            }
        }
    }
    Some(FileAnalysis {
        executable: walker.lines.difference(&excluded).copied().collect(),
        excluded,
    })
}

impl Walker<'_> {
    fn line(&self, offset: TextSize) -> u32 {
        self.index.line_index(offset).get() as u32
    }

    fn mark(&mut self, offset: TextSize) {
        let line = self.line(offset);
        self.lines.insert(line);
    }

    fn record_span(&mut self, stmt: &Stmt, header: TextSize) {
        self.spans.push(Span {
            header: self.line(header),
            start: self.line(stmt.range().start()),
            end: self.line(stmt.range().end()),
        });
    }

    fn visit_body(&mut self, body: &[Stmt]) {
        for stmt in body {
            self.visit_stmt(stmt);
        }
    }

    fn visit_stmt(&mut self, stmt: &Stmt) {
        let start = stmt.range().start();
        match stmt {
            Stmt::FunctionDef(def) => {
                for decorator in &def.decorator_list {
                    self.mark(decorator.range().start());
                }
                // The `def` line (its name token), not the first decorator.
                self.mark(def.name.range().start());
                self.record_span(stmt, def.name.range().start());
                self.visit_body(&def.body);
            }
            Stmt::ClassDef(def) => {
                for decorator in &def.decorator_list {
                    self.mark(decorator.range().start());
                }
                self.mark(def.name.range().start());
                self.record_span(stmt, def.name.range().start());
                self.visit_body(&def.body);
            }
            Stmt::If(if_stmt) => {
                self.mark(start);
                self.record_span(stmt, start);
                self.visit_body(&if_stmt.body);
                for clause in &if_stmt.elif_else_clauses {
                    self.visit_elif_else(clause);
                }
            }
            Stmt::While(while_stmt) => {
                self.mark(start);
                self.record_span(stmt, start);
                self.visit_body(&while_stmt.body);
                self.visit_body(&while_stmt.orelse);
            }
            Stmt::For(for_stmt) => {
                self.mark(start);
                self.record_span(stmt, start);
                self.visit_body(&for_stmt.body);
                self.visit_body(&for_stmt.orelse);
            }
            Stmt::With(with_stmt) => {
                self.mark(start);
                self.record_span(stmt, start);
                self.visit_body(&with_stmt.body);
            }
            Stmt::Try(try_stmt) => {
                self.mark(start);
                self.record_span(stmt, start);
                self.visit_body(&try_stmt.body);
                for handler in &try_stmt.handlers {
                    let ExceptHandler::ExceptHandler(handler) = handler;
                    self.mark(handler.range().start());
                    self.visit_body(&handler.body);
                }
                self.visit_body(&try_stmt.orelse);
                self.visit_body(&try_stmt.finalbody);
            }
            Stmt::Match(match_stmt) => {
                self.mark(start);
                self.record_span(stmt, start);
                for case in &match_stmt.cases {
                    self.mark(case.range.start());
                    self.visit_body(&case.body);
                }
            }
            Stmt::Expr(expr_stmt) => {
                // Constant expression statements (docstrings, bare literals)
                // are optimized away by CPython and never produce events.
                if !is_constant_literal(&expr_stmt.value) {
                    self.mark(start);
                    self.record_span(stmt, start);
                }
            }
            // Compile-time directives: no bytecode, no events.
            Stmt::Global(_) | Stmt::Nonlocal(_) => {}
            // Annotation without value generates no code.
            Stmt::AnnAssign(ann) if ann.value.is_none() => {}
            _ => {
                self.mark(start);
                self.record_span(stmt, start);
            }
        }
    }

    fn visit_elif_else(&mut self, clause: &ElifElseClause) {
        if clause.test.is_some() {
            // elif: the test executes. (A bare else has no event.)
            self.mark(clause.range.start());
        }
        self.visit_body(&clause.body);
    }
}

fn is_constant_literal(expr: &Expr) -> bool {
    matches!(
        expr,
        Expr::StringLiteral(_)
            | Expr::BytesLiteral(_)
            | Expr::NumberLiteral(_)
            | Expr::BooleanLiteral(_)
            | Expr::NoneLiteral(_)
            | Expr::EllipsisLiteral(_)
    )
}
