//! Executable-line analysis: which lines of a source file can produce
//! sys.monitoring LINE events. This is the coverage denominator, the
//! counterpart of coverage.py's parser (simplified; documented deltas are
//! acceptable in v1).

use std::collections::{BTreeMap, BTreeSet};

use ruff_python_ast::{ExceptHandler, Expr, Stmt};
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
    branches: BTreeMap<u32, Vec<i64>>,
    loops: BTreeSet<u32>,
    multiline: BTreeMap<u32, u32>,
    /// Lines coverage.py excludes via AST rather than a pragma: leading
    /// docstrings (module/class/function). Merged into `excluded` so an
    /// import-time `__doc__ =` event can't re-add them via the covered union.
    stub_excluded: BTreeSet<u32>,
    /// `...`-body defs as `(ellipsis_line, first_line, end_line)`. coverage.py
    /// excludes the whole def (decorators..body) only when the source line of
    /// the `...` matches its stub regex — resolved against the source in
    /// `analyze` (a def whose `...` rides a bracketed return type like
    /// `]: ...` is *not* excluded, matching coverage.py).
    stub_candidates: Vec<(u32, u32, u32)>,
}

/// coverage.py's default stub exclusion (one of its DEFAULT_EXCLUDE entries):
/// a line that is `...`, optionally prefixed by a one-line def signature whose
/// parameter list closes with `)` on that same line. Matches `def f(): ...`,
/// `) -> int: ...`, and a bare `...`; does NOT match `]: ...`.
static STUB_LINE_RE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
    regex::Regex::new(r"^\s*(((async )?def .*?)?\)(\s*->.*?)?:\s*)?\.\.\.\s*(#|$)").unwrap()
});

/// Branch destination for "control leaves the enclosing scope".
pub const EXIT: i64 = -1;

pub struct FileAnalysis {
    pub executable: BTreeSet<u32>,
    /// Continuation line -> first line of its statement (coverage.py folds
    /// multi-line statements onto their first line, runtime events too).
    pub multiline: BTreeMap<u32, u32>,
    /// for/while header lines (their "iterate" jump lands on the header's
    /// own advance instructions, unlike if fall-throughs).
    pub loops: BTreeSet<u32>,
    /// Lines excluded via `# pragma: no cover` (kept so observed hits on
    /// excluded lines stay out of the denominator).
    pub excluded: BTreeSet<u32>,
    /// Branch points: header line -> destination lines ([body, onward],
    /// EXIT for leaving the scope), coverage.py's source-level arcs for
    /// if/while/for.
    pub branches: BTreeMap<u32, Vec<i64>>,
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
        branches: BTreeMap::new(),
        loops: BTreeSet::new(),
        multiline: BTreeMap::new(),
        stub_excluded: BTreeSet::new(),
        stub_candidates: Vec::new(),
    };
    walker.exclude_leading_docstring(&parsed.syntax().body);
    walker.visit_body(&parsed.syntax().body, EXIT);

    let source_lines: Vec<&str> = source.lines().collect();
    let line_text = |lineno: u32| source_lines.get(lineno as usize - 1).copied().unwrap_or("");

    let mut excluded: BTreeSet<u32> = walker.stub_excluded.clone();
    // A `...`-body def is excluded (decorators through body) only when the
    // source line of its `...` matches coverage.py's stub regex, mirroring
    // coverage.py exactly: `def f(): ...`, `) -> int: ...` and a bare `...`
    // exclude the whole def, but a bracketed return type ending `]: ...` does
    // not, so that def stays counted.
    for (ellipsis_line, first, end) in &walker.stub_candidates {
        if STUB_LINE_RE.is_match(line_text(*ellipsis_line)) {
            excluded.extend(*first..=*end);
        }
    }
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
    let branches = walker
        .branches
        .into_iter()
        .filter(|(line, _)| !excluded.contains(line))
        .collect();
    Some(FileAnalysis {
        executable: walker.lines.difference(&excluded).copied().collect(),
        loops: walker.loops,
        multiline: walker.multiline,
        excluded,
        branches,
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

    /// Exclude a body's leading docstring. coverage.py never counts a
    /// module/class/function docstring as a statement. A *function* docstring
    /// compiles to no bytecode (it lives in co_consts) and is already absent
    /// from the denominator, but a *module*- or *class*-level docstring
    /// compiles to a `__doc__ =` store that emits a runtime LINE event, which
    /// the covered-union (lib.rs) would otherwise re-add. Excluding its lines
    /// keeps all three out, matching coverage.py.
    fn exclude_leading_docstring(&mut self, body: &[Stmt]) {
        if let Some(Stmt::Expr(e)) = body.first()
            && matches!(&*e.value, Expr::StringLiteral(_))
        {
            let first = self.line(e.range().start());
            let last = self.line(e.range().end());
            self.stub_excluded.extend(first..=last);
        }
    }

    fn visit_body(&mut self, body: &[Stmt], after: i64) {
        for (i, stmt) in body.iter().enumerate() {
            let next = self.first_line(&body[i + 1..]).unwrap_or(after);
            self.visit_stmt(stmt, next);
        }
    }

    /// First line that produces an event in `body` (skips docstrings and
    /// other no-code statements), i.e. a branch destination.
    fn first_line(&self, body: &[Stmt]) -> Option<i64> {
        for stmt in body {
            match stmt {
                Stmt::Expr(e) if is_constant_literal(&e.value) => continue,
                Stmt::Global(_) | Stmt::Nonlocal(_) => continue,
                Stmt::AnnAssign(a) if a.value.is_none() => continue,
                Stmt::FunctionDef(def) => {
                    let offset = def
                        .decorator_list
                        .first()
                        .map(|d| d.range().start())
                        .unwrap_or_else(|| def.name.range().start());
                    return Some(self.line(offset) as i64);
                }
                Stmt::ClassDef(def) => {
                    let offset = def
                        .decorator_list
                        .first()
                        .map(|d| d.range().start())
                        .unwrap_or_else(|| def.name.range().start());
                    return Some(self.line(offset) as i64);
                }
                other => return Some(self.line(other.range().start()) as i64),
            }
        }
        None
    }

    /// Fold the lines of `range` onto the statement's first line.
    fn fold_multiline(&mut self, header: TextSize, range: ruff_text_size::TextRange) {
        let first = self.line(header);
        let last = self.line(range.end());
        for line in (first + 1)..=last {
            self.multiline.insert(line, first);
        }
    }

    fn add_branch(&mut self, line: u32, dests: [i64; 2]) {
        // A branch with both sides landing on the same line is no branch
        // (e.g. a one-line suite).
        if dests[0] != dests[1] {
            let entry = self.branches.entry(line).or_default();
            for dest in dests {
                if !entry.contains(&dest) {
                    entry.push(dest);
                }
            }
        }
    }

    fn visit_stmt(&mut self, stmt: &Stmt, next: i64) {
        let start = stmt.range().start();
        match stmt {
            Stmt::FunctionDef(def) => {
                // A `...`-body stub (overload/abstractmethod/Protocol/plain):
                // record it as a candidate. Whether coverage.py excludes the
                // whole def depends on the `...`'s source line (resolved in
                // `analyze`), so mark the def normally here — the exclusion, if
                // any, removes it afterward; otherwise it stays counted.
                if let Some(ellipsis) = stub_ellipsis(&def.body) {
                    let start_offset = def
                        .decorator_list
                        .first()
                        .map(|d| d.range().start())
                        .unwrap_or(start);
                    self.stub_candidates.push((
                        self.line(ellipsis.range().start()),
                        self.line(start_offset),
                        self.line(stmt.range().end()),
                    ));
                }
                for decorator in &def.decorator_list {
                    self.mark(decorator.range().start());
                }
                // The `def` line (its name token), not the first decorator.
                self.mark(def.name.range().start());
                // Fold a multi-line signature onto the def line so runtime LINE
                // events on its continuation lines (and an inline `...`/return
                // riding the closing line) don't inflate the count — coverage.py
                // folds multi-line statements the same way.
                let sig_end = def
                    .returns
                    .as_ref()
                    .map(|r| r.range())
                    .unwrap_or_else(|| def.parameters.range());
                self.fold_multiline(def.name.range().start(), sig_end);
                self.record_span(stmt, def.name.range().start());
                self.exclude_leading_docstring(&def.body);
                self.visit_body(&def.body, EXIT);
            }
            Stmt::ClassDef(def) => {
                for decorator in &def.decorator_list {
                    self.mark(decorator.range().start());
                }
                self.mark(def.name.range().start());
                self.record_span(stmt, def.name.range().start());
                self.exclude_leading_docstring(&def.body);
                self.visit_body(&def.body, EXIT);
            }
            Stmt::If(if_stmt) => {
                self.mark(start);
                self.record_span(stmt, start);
                self.fold_multiline(start, if_stmt.test.range());
                let line = self.line(start);
                // Chain: each tested clause branches to its body or onward
                // to the next clause / past the statement. Constant tests
                // compile the branch away (if True:), like coverage.py.
                let mut headers: Vec<(u32, &Expr, &[Stmt])> =
                    vec![(line, &if_stmt.test, &if_stmt.body)];
                let mut bare_else: Option<&[Stmt]> = None;
                for clause in &if_stmt.elif_else_clauses {
                    match &clause.test {
                        Some(test) => {
                            // elif: the test executes. (A bare else has no
                            // event.)
                            self.mark(clause.range.start());
                            headers.push((
                                self.line(clause.range.start()),
                                test,
                                clause.body.as_slice(),
                            ));
                        }
                        None => bare_else = Some(clause.body.as_slice()),
                    }
                }
                for (i, (header, test, body)) in headers.iter().enumerate() {
                    let onward = if let Some((next_header, _, _)) = headers.get(i + 1) {
                        *next_header as i64
                    } else if let Some(else_body) = bare_else {
                        self.first_line(else_body).unwrap_or(next)
                    } else {
                        next
                    };
                    if !is_constant_literal(test)
                        && let Some(body_first) = self.first_line(body)
                    {
                        self.add_branch(*header, [body_first, onward]);
                    }
                }
                for (_, _, body) in &headers {
                    self.visit_body(body, next);
                }
                if let Some(else_body) = bare_else {
                    self.visit_body(else_body, next);
                }
            }
            Stmt::While(while_stmt) => {
                self.mark(start);
                self.record_span(stmt, start);
                self.fold_multiline(start, while_stmt.test.range());
                let line = self.line(start);
                let exit_to = self.first_line(&while_stmt.orelse).unwrap_or(next);
                if !is_constant_literal(&while_stmt.test)
                    && let Some(body_first) = self.first_line(&while_stmt.body)
                {
                    self.add_branch(line, [body_first, exit_to]);
                    self.loops.insert(line);
                }
                // Falling off the loop body jumps back to the condition.
                self.visit_body(&while_stmt.body, line as i64);
                self.visit_body(&while_stmt.orelse, next);
            }
            Stmt::For(for_stmt) => {
                self.mark(start);
                self.record_span(stmt, start);
                self.fold_multiline(start, for_stmt.iter.range());
                let line = self.line(start);
                let exit_to = self.first_line(&for_stmt.orelse).unwrap_or(next);
                if let Some(body_first) = self.first_line(&for_stmt.body) {
                    self.add_branch(line, [body_first, exit_to]);
                    self.loops.insert(line);
                }
                self.visit_body(&for_stmt.body, line as i64);
                self.visit_body(&for_stmt.orelse, next);
            }
            Stmt::With(with_stmt) => {
                self.mark(start);
                self.record_span(stmt, start);
                if let Some(last_item) = with_stmt.items.last() {
                    self.fold_multiline(start, last_item.range());
                }
                self.visit_body(&with_stmt.body, next);
            }
            Stmt::Try(try_stmt) => {
                self.mark(start);
                self.record_span(stmt, start);
                self.visit_body(&try_stmt.body, next);
                for handler in &try_stmt.handlers {
                    let ExceptHandler::ExceptHandler(handler) = handler;
                    self.mark(handler.range().start());
                    self.visit_body(&handler.body, next);
                }
                self.visit_body(&try_stmt.orelse, next);
                self.visit_body(&try_stmt.finalbody, next);
            }
            Stmt::Match(match_stmt) => {
                self.mark(start);
                self.record_span(stmt, start);
                self.fold_multiline(start, match_stmt.subject.range());
                for case in &match_stmt.cases {
                    self.mark(case.range.start());
                    self.visit_body(&case.body, next);
                }
            }
            Stmt::Expr(expr_stmt) => {
                // coverage.py counts a bare expression statement, including a
                // constant one (a PEP 257 attribute docstring, or a bare
                // string/number/bool); CPython 3.13 and 3.14 both emit a line
                // event for it. Only `...` is exempt (excluded via the stub
                // regex), and a leading docstring is dropped separately by
                // exclude_leading_docstring.
                if !matches!(&*expr_stmt.value, Expr::EllipsisLiteral(_)) {
                    self.mark(start);
                    self.record_span(stmt, start);
                    self.fold_multiline(start, stmt.range());
                }
            }
            // Compile-time directives: no bytecode, no events.
            Stmt::Global(_) | Stmt::Nonlocal(_) => {}
            // Annotation without value generates no code.
            Stmt::AnnAssign(ann) if ann.value.is_none() => {}
            _ => {
                self.mark(start);
                self.record_span(stmt, start);
                self.fold_multiline(start, stmt.range());
            }
        }
    }
}

/// The `...` (Ellipsis) expression of a stub body: a single `...` statement,
/// optionally preceded by a docstring (overload/abstractmethod/Protocol/plain
/// stubs). Returns None for any other body. The caller decides whether the def
/// is excluded based on the `...`'s source line, matching coverage.py — a
/// `pass` or real-statement body is never a stub.
fn stub_ellipsis(body: &[Stmt]) -> Option<&Expr> {
    let mut stmts = body.iter();
    let mut current = stmts.next();
    // Skip a single leading docstring.
    if let Some(Stmt::Expr(e)) = current
        && matches!(&*e.value, Expr::StringLiteral(_))
    {
        current = stmts.next();
    }
    // Anything beyond the (optional docstring +) one statement disqualifies it.
    if stmts.next().is_some() {
        return None;
    }
    match current {
        Some(Stmt::Expr(e)) if matches!(&*e.value, Expr::EllipsisLiteral(_)) => {
            Some(e.value.as_ref())
        }
        _ => None,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn executable(source: &str) -> Vec<u32> {
        analyze(source, &[])
            .unwrap()
            .executable
            .into_iter()
            .collect()
    }

    #[test]
    fn ellipsis_stub_defs_are_excluded() {
        // overload / plain stub / Protocol-style method: only the real `def f`
        // and its body, plus the class header, count (coverage.py default).
        let src = "\
from typing import overload


@overload
def f(x: int) -> int: ...
@overload
def f(x: str) -> str: ...
def f(x):
    return x


def plain_stub() -> None: ...


class Base:
    def method(self) -> int: ...
";
        assert_eq!(executable(src), vec![1, 8, 9, 15]);
    }

    #[test]
    fn docstring_then_ellipsis_is_a_stub() {
        let src = "def f():\n    \"\"\"doc\"\"\"\n    ...\n";
        assert!(executable(src).is_empty());
    }

    #[test]
    fn pass_body_is_not_a_stub() {
        // `pass` is real bytecode; coverage.py counts both lines.
        assert_eq!(executable("def f():\n    pass\n"), vec![1, 2]);
    }

    #[test]
    fn body_with_real_statement_is_not_a_stub() {
        assert_eq!(executable("def f():\n    return 1\n"), vec![1, 2]);
    }

    #[test]
    fn bare_ellipsis_body_excludes_the_def() {
        // `def b():` + a bare `...` on its own line: coverage.py excludes the
        // whole def, so only `real`/`return` count.
        let src = "def b():\n    ...\ndef real():\n    return 1\n";
        assert_eq!(executable(src), vec![3, 4]);
    }

    #[test]
    fn multiline_stub_excluded_only_when_ellipsis_line_matches() {
        // `d` ends `) -> int: ...` (matches coverage.py's stub regex) -> the
        // whole def is excluded. `e` ends `]: ...` (a bracketed return type,
        // which does NOT match) -> the def line stays counted, like
        // coverage.py. Only line 1 (import) and line 5 (`def e(`) count.
        let src = "\
from typing import overload


def d(
    x: int,
) -> int: ...
";
        // Wrong indices below would catch a regression; `d` is fully excluded.
        assert_eq!(executable(src), vec![1]);
        let src_e = "\
from typing import overload


def e(
    x: int,
) -> tuple[
    int,
]: ...
";
        assert_eq!(executable(src_e), vec![1, 4]);
    }

    #[test]
    fn module_docstring_is_excluded() {
        // coverage.py counts only `X = 1`, not the module docstring.
        assert_eq!(
            executable("\"\"\"Module docstring.\"\"\"\nX = 1\n"),
            vec![2]
        );
    }

    #[test]
    fn class_docstring_is_excluded() {
        // `class C:` and `Y = 1` count; the class docstring does not.
        assert_eq!(
            executable("class C:\n    \"\"\"Class docstring.\"\"\"\n    Y = 1\n"),
            vec![1, 3]
        );
    }

    #[test]
    fn function_docstring_is_excluded() {
        // `def g():` and `return 1` count; the function docstring does not.
        assert_eq!(
            executable("def g():\n    \"\"\"Function docstring.\"\"\"\n    return 1\n"),
            vec![1, 3]
        );
    }

    #[test]
    fn multiline_module_docstring_is_excluded() {
        assert_eq!(
            executable("\"\"\"line one\nline two\n\"\"\"\nX = 1\n"),
            vec![4]
        );
    }

    #[test]
    fn bare_constant_statements_are_counted_except_ellipsis() {
        // coverage.py counts a bare constant statement — a PEP 257 attribute
        // docstring or a bare string/number/bool — on 3.13 and 3.14 alike;
        // only `...` and a leading docstring are dropped.
        let src = "\
class C:
    X = 1
    \"\"\"attribute docstring\"\"\"
    Y = 2
";
        // `class C`, `X = 1`, the attribute docstring, and `Y = 2` all count.
        assert_eq!(executable(src), vec![1, 2, 3, 4]);
        // A bare module-level `...` is excluded; the assignment around it counts.
        assert_eq!(executable("A = 1\n...\nB = 2\n"), vec![1, 3]);
    }

    #[test]
    fn statement_free_files_have_no_executable_lines() {
        // The lib.rs covered-union keys on an empty `executable` to report
        // such files as 0/0 (matching coverage.py) instead of letting an
        // import-time phantom LINE event invent a statement.
        assert!(executable("").is_empty());
        assert!(executable("# just a comment\n").is_empty());
        assert!(executable("\"\"\"Only a module docstring.\"\"\"\n").is_empty());
        assert!(executable("def f() -> int: ...\n").is_empty());
    }
}
