//! Executable-line analysis: which lines of a source file can produce
//! sys.monitoring LINE events. This is the coverage denominator, the
//! counterpart of coverage.py's parser (simplified; documented deltas are
//! acceptable in v1).

use std::collections::{BTreeMap, BTreeSet};

use ruff_python_ast::helpers::any_over_expr;
use ruff_python_ast::{ExceptHandler, Expr, Stmt};
use ruff_source_file::LineIndex;
use ruff_text_size::{Ranged, TextSize};

/// A statement span used for `# pragma: no cover` block exclusion.
struct Span {
    header: u32,
    start: u32,
    end: u32,
}

/// The immediate scope of the body being walked — it decides whether a bare
/// annotation (`x: T`, no value) is a counted statement.
#[derive(Clone, Copy, PartialEq)]
enum BodyScope {
    Module,
    Class,
    Function,
}

/// A `case` clause's pattern lines and body lines (first, last).
struct CaseLines {
    pattern: (u32, u32),
    body: (u32, u32),
}

struct Walker<'a> {
    index: &'a LineIndex,
    lines: BTreeSet<u32>,
    spans: Vec<Span>,
    branches: BTreeMap<u32, Vec<i64>>,
    withs: BTreeMap<u32, (u32, i64)>,
    multiline: BTreeMap<u32, u32>,
    /// Lines coverage.py excludes via AST rather than a pragma: leading
    /// docstrings (module/class/function). Merged into `excluded` so an
    /// import-time `__doc__ =` event can't re-add them via the covered union.
    stub_excluded: BTreeSet<u32>,
    /// `...`-body defs as `(ellipsis_line, first_line, end_line)`. coverage.py
    /// excludes the whole def (decorators..body) only when the source line of
    /// the `...` matches its stub regex — resolved against the source in
    /// `analyze_for`.
    stub_candidates: Vec<(u32, u32, u32)>,
    /// Whether coverage.py's default `...`-stub exclusion applies (no custom
    /// `exclude_lines`); without it a `...` statement counts like any other.
    default_stubs: bool,
    /// Irrefutable `case` clauses, whose `case` line coverage.py excludes
    /// along with an entirely excluded body.
    irrefutable_cases: Vec<CaseLines>,
    /// The immediate scope of the body being walked. Decides whether a bare
    /// annotation (`x: T`, no value) counts: module-level always does;
    /// function-level never does (locals aren't annotated); class-level depends
    /// on `future_annotations` / a nested code object (see the AnnAssign arm).
    scope: BodyScope,
    /// Whether the file has `from __future__ import annotations`. Under PEP 563
    /// a class annotation stores its stringized form into `__annotations__`
    /// eagerly (bytecode on that line), so coverage.py counts it; under PEP 649
    /// (Python 3.14 default, no future import) class annotations are lazy in a
    /// separate `__annotate__` object and coverage.py counts a line only when
    /// the annotation builds a nested code object (a lambda/generator).
    future_annotations: bool,
    /// Python 3.14+: class-level bare annotations are evaluated lazily.
    lazy_annotations: bool,
}

/// A line that is only `...` (and maybe a comment).
static BARE_ELLIPSIS_RE: std::sync::LazyLock<regex::Regex> =
    std::sync::LazyLock::new(|| regex::Regex::new(r"^\s*\.\.\.\s*(#|$)").expect("valid regex"));

/// coverage.py's default stub exclusion (one of its DEFAULT_EXCLUDE entries):
/// a line that is `...`, optionally prefixed by the end of a signature
/// closing with `)` or `]` on that same line. Matches `def f(): ...`,
/// `) -> int: ...`, `]: ...` and a bare `...`.
static STUB_LINE_RE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
    regex::Regex::new(r"^\s*(((async )?def .*?)?[\])]+(\s*->.*?)?:\s*)?\.\.\.\s*(#|$)").unwrap()
});

/// Branch destination for "control leaves the enclosing scope".
pub const EXIT: i64 = -1;

pub struct FileAnalysis {
    pub executable: BTreeSet<u32>,
    /// Continuation line -> first line of its statement (coverage.py folds
    /// multi-line statements onto their first line, runtime events too).
    pub multiline: BTreeMap<u32, u32>,
    /// Lines excluded via `# pragma: no cover` (kept so observed hits on
    /// excluded lines stay out of the denominator).
    pub excluded: BTreeSet<u32>,
    /// Branch points: header line -> destination lines ([body, onward],
    /// EXIT for leaving the scope), coverage.py's source-level arcs for
    /// if/while/for.
    pub branches: BTreeMap<u32, Vec<i64>>,
    /// `with` statements: header line -> (last line of the body, where
    /// execution continues after it). Leaving the body jumps back to the
    /// header to run `__exit__`; coverage.py presents that jump as going
    /// on to the next statement.
    pub withs: BTreeMap<u32, (u32, i64)>,
}

/// coverage.py's default exclude_lines regex (`# pragma: no cover`).
pub const DEFAULT_EXCLUDE: &str = r"#\s*(pragma|PRAGMA)[:\s]?\s*(no|NO)\s*(cover|COVER)";

/// coverage.py's other default exclusion: an `if TYPE_CHECKING:` block, whose
/// body only runs under a type checker. (`...`-stub exclusion, the third
/// default, is handled structurally in the AST walk instead of as a regex.)
pub const DEFAULT_EXCLUDE_TYPE_CHECKING: &str = r"if (typing\.)?TYPE_CHECKING:";

/// The effective exclusions: coverage.py's `exclude_list` regexes, and
/// whether its default list is in force. A custom `exclude_lines` replaces
/// the whole default list, so the `...`-stub exclusion (applied structurally
/// here rather than as a regex) goes with it.
#[derive(Default)]
pub struct ExcludeRules {
    pub patterns: Vec<regex::Regex>,
    pub default_stubs: bool,
}

/// The executable lines of `source`, or None if it does not parse.
/// `excludes` are the effective exclude_lines regexes: a match on a
/// statement's header line excludes the whole statement.
#[cfg(test)]
pub fn analyze(source: &str, excludes: &[regex::Regex]) -> Option<FileAnalysis> {
    let rules = ExcludeRules {
        patterns: excludes.to_vec(),
        default_stubs: true,
    };
    analyze_for(source, &rules, true)
}

/// [`analyze`] for a given Python: `lazy_annotations` (3.14+, PEP 649) when
/// class-level bare annotations compile to no code of their own.
pub fn analyze_for(
    source: &str,
    rules: &ExcludeRules,
    lazy_annotations: bool,
) -> Option<FileAnalysis> {
    let excludes = &rules.patterns;
    let parsed = ruff_python_parser::parse_module(source).ok()?;
    let index = LineIndex::from_source_text(source);
    let mut walker = Walker {
        index: &index,
        lines: BTreeSet::new(),
        spans: Vec::new(),
        branches: BTreeMap::new(),
        withs: BTreeMap::new(),
        multiline: BTreeMap::new(),
        stub_excluded: BTreeSet::new(),
        stub_candidates: Vec::new(),
        default_stubs: rules.default_stubs,
        irrefutable_cases: Vec::new(),
        scope: BodyScope::Module,
        future_annotations: module_has_future_annotations(&parsed.syntax().body),
        lazy_annotations,
    };
    walker.exclude_leading_docstring(&parsed.syntax().body);
    walker.visit_body(&parsed.syntax().body, EXIT);

    let source_lines: Vec<&str> = source.lines().collect();
    let line_text = |lineno: u32| source_lines.get(lineno as usize - 1).copied().unwrap_or("");

    let mut excluded: BTreeSet<u32> = walker.stub_excluded.clone();
    // A `...`-body def is excluded (decorators through body) only when the
    // source line of its `...` matches coverage.py's stub regex, mirroring
    // coverage.py exactly: `def f(): ...`, `) -> int: ...`, `]: ...` and a
    // bare `...` exclude the whole def; a `...` the regex does not match
    // (after a docstring on the same line, say) leaves it counted.
    for (ellipsis_line, first, end) in &walker.stub_candidates {
        if rules.default_stubs && STUB_LINE_RE.is_match(line_text(*ellipsis_line)) {
            excluded.extend(*first..=*end);
        }
    }
    for (lineno, line) in source.lines().enumerate() {
        // A bare `...` line: coverage.py's default exclusion (its stub
        // regex) matches it wherever it is, not only in a stub def.
        if rules.default_stubs && BARE_ELLIPSIS_RE.is_match(line) {
            excluded.insert((lineno + 1) as u32);
        }
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
    // An irrefutable `case` (`case _:`) is the `match` equivalent of an
    // `else`: when its whole body is excluded, so is the `case` line.
    for case in &walker.irrefutable_cases {
        let mut body = walker.lines.range(case.body.0..=case.body.1).peekable();
        if body.peek().is_some() && body.all(|line| excluded.contains(line)) {
            excluded.extend(case.pattern.0..=case.pattern.1);
        }
    }
    // Arcs into excluded lines do not exist for coverage.py, so a branch
    // left with a single destination is no branch.
    let branches = walker
        .branches
        .into_iter()
        .filter(|(line, _)| !excluded.contains(line))
        .map(|(line, dests)| {
            let dests: Vec<i64> = dests
                .into_iter()
                .filter(|dest| u32::try_from(*dest).map_or(true, |d| !excluded.contains(&d)))
                .collect();
            (line, dests)
        })
        .filter(|(_, dests)| dests.len() > 1)
        .collect();
    let mut multiline = logical_lines(parsed.tokens(), &index);
    for (line, first) in walker.multiline {
        multiline.entry(line).or_insert(first);
    }
    Some(FileAnalysis {
        executable: walker.lines.difference(&excluded).copied().collect(),
        multiline,
        excluded,
        branches,
        withs: walker.withs,
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
            // The compiler drops the statements after an unconditional
            // jump in the same block: no statements (coverage.py reads them
            // from the bytecode).
            if matches!(
                stmt,
                Stmt::Return(_) | Stmt::Raise(_) | Stmt::Continue(_) | Stmt::Break(_)
            ) && let (Some(first), Some(last)) = (body.get(i + 1), body.last())
            {
                let (first, last) = (
                    self.line(first.range().start()),
                    self.line(last.range().end()),
                );
                self.stub_excluded.extend(first..=last);
                break;
            }
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

    /// coverage.py's `_case_is_irrefutable`: no guard, and (through the last
    /// alternative of an or-pattern and any `as` wrappers) a wildcard or a
    /// bare capture.
    fn case_is_irrefutable(case: &ruff_python_ast::MatchCase) -> bool {
        use ruff_python_ast::Pattern;
        if case.guard.is_some() {
            return false;
        }
        let mut pattern = &case.pattern;
        loop {
            match pattern {
                Pattern::MatchOr(or) => match or.patterns.last() {
                    Some(last) => pattern = last,
                    None => return false,
                },
                Pattern::MatchAs(as_) => match &as_.pattern {
                    Some(inner) => pattern = inner,
                    None => return true,
                },
                _ => return false,
            }
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
                let prev = self.scope;
                self.scope = BodyScope::Function;
                self.visit_body(&def.body, EXIT);
                self.scope = prev;
            }
            Stmt::ClassDef(def) => {
                for decorator in &def.decorator_list {
                    self.mark(decorator.range().start());
                }
                self.mark(def.name.range().start());
                self.record_span(stmt, def.name.range().start());
                self.exclude_leading_docstring(&def.body);
                // A class body is its own annotation scope, even nested in a
                // function.
                let prev = self.scope;
                self.scope = BodyScope::Class;
                self.visit_body(&def.body, EXIT);
                self.scope = prev;
            }
            Stmt::If(if_stmt) => {
                self.mark(start);
                // An exclusion matching the `if` header (e.g. a `# pragma` or
                // `if TYPE_CHECKING:`) drops the test and the if-body, but not
                // any elif/else clauses — those still run. So the span ends at
                // the if-body, not at the whole statement (which would swallow
                // a runtime `else:`).
                let if_body_end = if_stmt
                    .body
                    .last()
                    .map(|s| self.line(s.range().end()))
                    .unwrap_or_else(|| self.line(start));
                self.spans.push(Span {
                    header: self.line(start),
                    start: self.line(start),
                    end: if_body_end,
                });
                self.fold_multiline(start, if_stmt.test.range());
                let line = self.line(start);
                // Chain: each tested clause branches to its body or onward
                // to the next clause / past the statement. Constant tests
                // compile the branch away (if True:), like coverage.py.
                let mut headers: Vec<(u32, &Expr, &[Stmt])> =
                    vec![(line, &if_stmt.test, &if_stmt.body)];
                let mut bare_else: Option<&[Stmt]> = None;
                for clause in &if_stmt.elif_else_clauses {
                    // Each elif/else clause gets its own span, so a `# pragma`
                    // on the clause line excludes that clause's body (the if's
                    // own span covers only the if-body).
                    let clause_line = self.line(clause.range.start());
                    let clause_end = clause
                        .body
                        .last()
                        .map(|s| self.line(s.range().end()))
                        .unwrap_or(clause_line);
                    self.spans.push(Span {
                        header: clause_line,
                        start: clause_line,
                        end: clause_end,
                    });
                    match &clause.test {
                        Some(test) => {
                            // elif: the test executes. (A bare else has no
                            // event.)
                            self.mark(clause.range.start());
                            headers.push((clause_line, test, clause.body.as_slice()));
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
                for (_, test, body) in &headers {
                    // `if not __debug__:` compiles to nothing: its body is
                    // no statement (coverage.py reads statements from the
                    // bytecode).
                    if is_debug_test(test) == Some(false) {
                        if let (Some(first), Some(last)) = (body.first(), body.last()) {
                            let (first, last) = (
                                self.line(first.range().start()),
                                self.line(last.range().end()),
                            );
                            self.stub_excluded.extend(first..=last);
                        }
                        continue;
                    }
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
                }
                self.visit_body(&for_stmt.body, line as i64);
                self.visit_body(&for_stmt.orelse, next);
            }
            Stmt::With(with_stmt) => {
                self.withs
                    .insert(self.line(start), (self.line(stmt.range().end()), next));
                self.mark(start);
                self.record_span(stmt, start);
                if let Some(last_item) = with_stmt.items.last() {
                    self.fold_multiline(start, last_item.range());
                }
                self.visit_body(&with_stmt.body, next);
            }
            Stmt::Try(try_stmt) => {
                self.mark(start);
                // The try's own span covers only the try-body, so a `# pragma`
                // on `try:` doesn't swallow the handlers/else/finally (which
                // still run); each clause below gets its own span.
                let try_body_end = try_stmt
                    .body
                    .last()
                    .map(|s| self.line(s.range().end()))
                    .unwrap_or_else(|| self.line(start));
                self.spans.push(Span {
                    header: self.line(start),
                    start: self.line(start),
                    end: try_body_end,
                });
                self.visit_body(&try_stmt.body, next);
                for handler in &try_stmt.handlers {
                    let ExceptHandler::ExceptHandler(handler) = handler;
                    self.mark(handler.range().start());
                    // Per-handler span: a `# pragma` on `except ...:` excludes
                    // that handler's body.
                    let h_line = self.line(handler.range().start());
                    let h_end = handler
                        .body
                        .last()
                        .map(|s| self.line(s.range().end()))
                        .unwrap_or(h_line);
                    self.spans.push(Span {
                        header: h_line,
                        start: h_line,
                        end: h_end,
                    });
                    self.visit_body(&handler.body, next);
                }
                self.visit_body(&try_stmt.orelse, next);
                self.visit_body(&try_stmt.finalbody, next);
            }
            Stmt::Match(match_stmt) => {
                self.mark(start);
                self.record_span(stmt, start);
                self.fold_multiline(start, match_stmt.subject.range());
                for (i, case) in match_stmt.cases.iter().enumerate() {
                    self.mark(case.range.start());
                    // A refutable case branches to its body or onward to the
                    // next case (past the match after the last one); an
                    // irrefutable one (`case _:`, a bare capture, no guard)
                    // always enters its body.
                    let irrefutable = case.guard.is_none()
                        && matches!(
                            &case.pattern,
                            ruff_python_ast::Pattern::MatchAs(m) if m.pattern.is_none()
                        );
                    if !irrefutable && let Some(body_first) = self.first_line(&case.body) {
                        let onward = match_stmt
                            .cases
                            .get(i + 1)
                            .map_or(next, |c| i64::from(self.line(c.range.start())));
                        self.add_branch(self.line(case.range.start()), [body_first, onward]);
                    }
                    // Per-case span: a `# pragma` on a `case ...:` line excludes
                    // that case's body (e.g. `case _: assert_never(x)`).
                    let case_line = self.line(case.range.start());
                    if Self::case_is_irrefutable(case)
                        && let (Some(first), Some(last)) = (case.body.first(), case.body.last())
                    {
                        self.irrefutable_cases.push(CaseLines {
                            pattern: (
                                self.line(case.pattern.range().start()),
                                self.line(case.pattern.range().end()),
                            ),
                            body: (
                                self.line(first.range().start()),
                                self.line(last.range().end()),
                            ),
                        });
                    }
                    let case_end = case
                        .body
                        .last()
                        .map(|s| self.line(s.range().end()))
                        .unwrap_or(case_line);
                    self.spans.push(Span {
                        header: case_line,
                        start: case_line,
                        end: case_end,
                    });
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
                if !self.default_stubs || !matches!(&*expr_stmt.value, Expr::EllipsisLiteral(_)) {
                    self.mark(start);
                    self.record_span(stmt, start);
                    self.fold_multiline(start, stmt.range());
                }
            }
            // Compile-time directives: no bytecode, no events.
            Stmt::Global(_) | Stmt::Nonlocal(_) => {}
            // A bare annotation (`x: T`, no value). coverage.py counts it:
            //  - at module scope: always;
            //  - at class scope: before Python 3.14 (eagerly stored); on
            //    3.14+ under `from __future__ import annotations`
            //    (eager string store), or when the annotation builds a nested
            //    code object — a lambda/generator — whose line PEP 649's lazy
            //    `__annotate__` still emits (a plain `x: int` does not);
            //  - never inside a function (locals aren't annotated).
            Stmt::AnnAssign(ann) if ann.value.is_none() => {
                let counts = match self.scope {
                    BodyScope::Module => true,
                    BodyScope::Class => {
                        !self.lazy_annotations
                            || self.future_annotations
                            || any_over_expr(&ann.annotation, |e| {
                                matches!(e, Expr::Lambda(_) | Expr::Generator(_))
                            })
                    }
                    BodyScope::Function => false,
                };
                if counts {
                    self.mark(start);
                    self.record_span(stmt, start);
                    self.fold_multiline(start, stmt.range());
                }
            }
            _ => {
                self.mark(start);
                self.record_span(stmt, start);
                self.fold_multiline(start, stmt.range());
            }
        }
    }
}

/// Whether the module begins with `from __future__ import annotations`
/// (future imports must precede other code, so only the leading run of
/// imports needs checking).
fn module_has_future_annotations(body: &[Stmt]) -> bool {
    for stmt in body {
        match stmt {
            // A leading module docstring is allowed before future imports.
            Stmt::Expr(e) if matches!(&*e.value, Expr::StringLiteral(_)) => continue,
            Stmt::ImportFrom(import) => {
                if import
                    .module
                    .as_ref()
                    .is_some_and(|m| m.as_str() == "__future__")
                    && import
                        .names
                        .iter()
                        .any(|n| n.name.as_str() == "annotations")
                {
                    return true;
                }
            }
            // Future imports must come first; once real code starts, stop.
            _ => break,
        }
    }
    false
}

/// The `...` expression of a stub body whose *sole* statement is `...`
/// (overload/abstractmethod/Protocol/plain stubs). Returns None for any other
/// body — including a docstring followed by `...`, which coverage.py counts
/// (the def line stays a statement; only the `...` and docstring drop). The
/// caller decides exclusion from the `...`'s source line.
fn stub_ellipsis(body: &[Stmt]) -> Option<&Expr> {
    match body {
        [Stmt::Expr(e)] if matches!(&*e.value, Expr::EllipsisLiteral(_)) => Some(e.value.as_ref()),
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
    ) || is_debug_test(expr).is_some()
}

/// `__debug__` / `not __debug__`: constants the compiler folds (True
/// without -O), so `if not __debug__:` compiles to no code at all.
fn is_debug_test(expr: &Expr) -> Option<bool> {
    match expr {
        Expr::Name(name) if name.id.as_str() == "__debug__" => Some(true),
        Expr::UnaryOp(op) if op.op == ruff_python_ast::UnaryOp::Not => {
            is_debug_test(&op.operand).map(|v| !v)
        }
        _ => None,
    }
}

/// coverage.py's multiline map, from the tokens: every physical line of a
/// logical line (a statement, a decorator, a header) maps to its first.
fn logical_lines(
    tokens: &[ruff_python_ast::token::Token],
    index: &LineIndex,
) -> BTreeMap<u32, u32> {
    use ruff_python_ast::token::TokenKind;
    let mut map = BTreeMap::new();
    let mut first: Option<u32> = None;
    for token in tokens {
        match token.kind() {
            TokenKind::Newline => {
                if let Some(first_line) = first.take() {
                    let last = index.line_index(token.range().start()).get() as u32;
                    for line in (first_line + 1)..=last {
                        map.insert(line, first_line);
                    }
                }
            }
            TokenKind::NonLogicalNewline
            | TokenKind::Comment
            | TokenKind::Indent
            | TokenKind::Dedent => {}
            _ => {
                if first.is_none() {
                    first = Some(index.line_index(token.range().start()).get() as u32);
                }
            }
        }
    }
    map
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
    fn docstring_then_ellipsis_keeps_the_def_line() {
        // coverage.py excludes only a *sole* `...` body; a docstring before it
        // keeps the def line counted (the docstring and `...` still drop).
        let src = "def f():\n    \"\"\"doc\"\"\"\n    ...\n";
        assert_eq!(executable(src), vec![1]);
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
        // `d` ends `) -> int: ...` and `e` ends `]: ...` (a bracketed return
        // type): both match coverage.py's stub regex, so the whole def is
        // excluded and only line 1 (import) counts.
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
        assert_eq!(executable(src_e), vec![1]);
    }

    #[test]
    fn custom_exclude_lines_drop_the_stub_defaults() {
        // A custom exclude_lines replaces coverage.py's whole default list:
        // a `...` body and a one-line stub are statements again.
        let src = "\
class P:
    def code(self):
        \"\"\"The code.\"\"\"
        ...

    def get(self, k): ...
";
        let rules = ExcludeRules {
            patterns: vec![regex::Regex::new("pragma: no cover").unwrap()],
            default_stubs: false,
        };
        let custom: Vec<u32> = analyze_for(src, &rules, true)
            .unwrap()
            .executable
            .into_iter()
            .collect();
        assert_eq!(custom, vec![1, 2, 4, 6]);
        assert_eq!(executable(src), vec![1, 2]);
    }

    #[test]
    fn irrefutable_case_with_an_excluded_body_is_excluded() {
        let src = "\
def name(x):
    match x:
        case 1:
            return \"one\"
        case _:
            raise NotImplementedError
";
        let excludes = [regex::Regex::new("raise NotImplementedError").unwrap()];
        let lines: Vec<u32> = analyze(src, &excludes)
            .unwrap()
            .executable
            .into_iter()
            .collect();
        assert_eq!(lines, vec![1, 2, 3, 4]);
        // A refutable case keeps its line, like an `elif`.
        let refutable = src.replace("case _:", "case 2:");
        let lines: Vec<u32> = analyze(&refutable, &excludes)
            .unwrap()
            .executable
            .into_iter()
            .collect();
        assert_eq!(lines, vec![1, 2, 3, 4, 5]);
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
    fn bare_annotation_counts_by_scope_without_future() {
        // No `from __future__ import annotations` (PEP 649 lazy). coverage.py:
        // module annotation counts; class annotation counts only if it builds
        // a nested code object (lambda/generator); function-local never counts.
        let src = "\
module_ann: int
module_lambda: Annotated[int, lambda v: v]


class C:
    class_ann: int
    class_lambda: Annotated[int, lambda v: v]
    class_with_val: int = 1


def f() -> None:
    local_ann: int
";
        assert_eq!(executable(src), vec![1, 2, 5, 7, 8, 11]);
    }

    #[test]
    fn bare_class_annotation_counts_under_future_import() {
        // PEP 563: class annotations are stored eagerly, so they count.
        let src = "from __future__ import annotations\n\n\nclass C:\n    a: int\n    b: str\n";
        assert_eq!(executable(src), vec![1, 4, 5, 6]);
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

    #[test]
    fn continuation_lines_fold_onto_their_logical_line() {
        let src = "\
@decorate(
    a=1,
    b=2,
)
def f(x):
    if (x
        and x > 1):
        return 1
    return 2


class C(
    Base,
):
    pass
";
        let analysis = analyze(src, &[]).unwrap();
        // Each continuation line maps to the first line of its logical line,
        // so runtime events on them fold away (coverage.py's multiline map).
        assert_eq!(analysis.multiline.get(&2), Some(&1));
        assert_eq!(analysis.multiline.get(&4), Some(&1));
        assert_eq!(analysis.multiline.get(&7), Some(&6));
        assert_eq!(analysis.multiline.get(&13), Some(&12));
        assert_eq!(analysis.multiline.get(&5), None);
    }

    #[test]
    fn if_not_debug_body_is_no_statement() {
        let src = "\
def f():
    if not __debug__:
        return
    x = 1
    if __debug__:
        y = 2
";
        let analysis = analyze(src, &[]).unwrap();
        assert!(
            !analysis.executable.contains(&3),
            "{:?}",
            analysis.executable
        );
        assert!(analysis.executable.contains(&4));
        assert!(analysis.executable.contains(&6));
        // Constant tests are no branches.
        assert!(analysis.branches.is_empty(), "{:?}", analysis.branches);
    }

    #[test]
    fn statements_after_a_return_are_dropped() {
        let src = "\
def gen():
    return
    yield
";
        assert_eq!(executable(src), vec![1, 2]);
    }

    #[test]
    fn class_annotations_count_before_python_3_14() {
        let src = "\
class C:
    x: int
    y: int = 1
";
        let defaults = ExcludeRules {
            patterns: Vec::new(),
            default_stubs: true,
        };
        let eager = analyze_for(src, &defaults, false).unwrap().executable;
        let lazy = analyze_for(src, &defaults, true).unwrap().executable;
        assert!(eager.contains(&2));
        assert!(!lazy.contains(&2));
        assert!(lazy.contains(&3));
    }

    #[test]
    fn refutable_cases_branch() {
        let src = "\
def f(m):
    match m:
        case 1:
            a = 1
        case str() if m:
            a = 2
        case _:
            a = 3
    return a
";
        let analysis = analyze(src, &[]).unwrap();
        assert_eq!(analysis.branches.get(&3), Some(&vec![4, 5]));
        assert_eq!(analysis.branches.get(&5), Some(&vec![6, 7]));
        assert_eq!(analysis.branches.get(&7), None);
    }

    #[test]
    fn a_branch_into_an_excluded_line_is_no_branch() {
        let src = "\
def f(x):
    if x:
        y = 1
    if DEBUG:  # pragma: no cover
        z = 2
";
        let excludes = [regex::Regex::new(DEFAULT_EXCLUDE).unwrap()];
        let analysis = analyze(src, &excludes).unwrap();
        assert_eq!(analysis.branches.get(&2), None);
    }
}
