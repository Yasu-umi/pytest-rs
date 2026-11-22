//! Code units and their checksums (docs/affected.md).
//!
//! A Python file splits into one unit per `def`/`async def` (keyed by its
//! qualified name), one per `class`, and one module unit. Checksums hash
//! ruff's position-free `ComparableStmt`, so formatting and comments never
//! change a unit, and units are keyed by name rather than position, so
//! inserting or reordering definitions changes nothing.
//!
//! What each unit covers:
//! - function: decorators, signature and body; a nested `def`/`class` in the
//!   body is replaced by a name-only placeholder (it is a unit of its own);
//! - class: decorators, bases and the body statements; a method or nested
//!   class is a name-only placeholder (adding, removing or renaming one
//!   changes the class, which covers overrides of inherited methods);
//! - module: the top-level statements other than `def` and `class`, which
//!   are left out entirely (adding a function changes nothing).

use std::collections::HashMap;
use std::hash::{Hash, Hasher};

use ruff_python_ast::comparable::ComparableStmt;
use ruff_python_ast::{self as ast, ExceptHandler, Stmt};
use ruff_text_size::TextRange;
use siphasher::sip::SipHasher13;

use crate::names::ModuleFacts;

/// The qualname of a file's module unit (matches `co_qualname` of a
/// module's code object).
pub const MODULE_UNIT: &str = "<module>";

/// A pseudo unit covering a whole file by content hash: the fallback for a
/// code object whose qualname matches no unit of its file.
pub const FILE_UNIT: &str = "<file>";

/// Bumped whenever the unit split or the hash input changes; stored in the
/// data header, where a mismatch means a full re-record.
pub const CHECKSUM_SCHEME: u32 = 2;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unit {
    pub qualname: String,
    pub checksum: u64,
}

/// All units of one file, by qualname.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FileUnits {
    pub units: HashMap<String, u64>,
}

impl FileUnits {
    /// Split a source file. None when it does not parse: every unit of the
    /// file then counts as changed.
    pub fn parse(source: &str) -> Option<Self> {
        let parsed = ruff_python_parser::parse_module(source).ok()?;
        Some(Self::from_suite(parsed.suite()))
    }

    /// The units of a parsed module, the name units of its module-level and
    /// class-level statements (`crate::names`) included.
    pub fn from_suite(suite: &[Stmt]) -> Self {
        let mut found: Vec<Unit> = Vec::new();
        Self::walk(suite, "", &mut found);
        found.push(Unit {
            qualname: MODULE_UNIT.to_string(),
            checksum: Self::hash_suite(&Self::strip_defs(suite)),
        });
        found.extend(ModuleFacts::analyze(suite, None, false).1);
        Self::merge(found)
    }

    /// Duplicate qualnames (`if X: def f() ... else: def f() ...`) merge
    /// into one unit whose checksum covers every definition, in order.
    fn merge(found: Vec<Unit>) -> Self {
        let mut grouped: Vec<(String, Vec<u64>)> = Vec::new();
        let mut index: HashMap<String, usize> = HashMap::new();
        for unit in found {
            match index.get(&unit.qualname) {
                Some(&i) => grouped[i].1.push(unit.checksum),
                None => {
                    index.insert(unit.qualname.clone(), grouped.len());
                    grouped.push((unit.qualname, vec![unit.checksum]));
                }
            }
        }
        let units = grouped
            .into_iter()
            .map(|(qualname, checksums)| {
                let checksum = match checksums.as_slice() {
                    [single] => *single,
                    many => {
                        let mut hasher = Self::hasher();
                        many.hash(&mut hasher);
                        hasher.finish()
                    }
                };
                (qualname, checksum)
            })
            .collect();
        Self { units }
    }

    /// Collect the function and class units under `prefix` (the qualname
    /// prefix, including its trailing separator).
    fn walk(stmts: &[Stmt], prefix: &str, found: &mut Vec<Unit>) {
        for stmt in stmts {
            match stmt {
                Stmt::FunctionDef(def) => {
                    let qualname = format!("{prefix}{}", def.name.id);
                    let mut stub = def.clone();
                    stub.body = Self::placeholder_defs(&def.body).into();
                    found.push(Unit {
                        qualname: qualname.clone(),
                        checksum: Self::hash_stmt(&Stmt::FunctionDef(stub)),
                    });
                    Self::walk(&def.body, &format!("{qualname}.<locals>."), found);
                }
                Stmt::ClassDef(class) => {
                    let qualname = format!("{prefix}{}", class.name.id);
                    let mut stub = class.clone();
                    stub.body = Self::placeholder_defs(&class.body).into();
                    found.push(Unit {
                        qualname: qualname.clone(),
                        checksum: Self::hash_stmt(&Stmt::ClassDef(stub)),
                    });
                    Self::walk(&class.body, &format!("{qualname}."), found);
                }
                other => {
                    for body in Self::child_suites(other) {
                        Self::walk(body, prefix, found);
                    }
                }
            }
        }
    }

    /// The statement lists nested in a compound statement other than
    /// `def`/`class`.
    pub(crate) fn child_suites(stmt: &Stmt) -> Vec<&[Stmt]> {
        match stmt {
            Stmt::If(node) => std::iter::once(node.body.as_slice())
                .chain(node.elif_else_clauses.iter().map(|c| c.body.as_slice()))
                .collect(),
            Stmt::For(node) => vec![node.body.as_slice(), node.orelse.as_slice()],
            Stmt::While(node) => vec![node.body.as_slice(), node.orelse.as_slice()],
            Stmt::With(node) => vec![node.body.as_slice()],
            Stmt::Try(node) => std::iter::once(node.body.as_slice())
                .chain(node.handlers.iter().map(|h| match h {
                    ExceptHandler::ExceptHandler(h) => h.body.as_slice(),
                }))
                .chain([node.orelse.as_slice(), node.finalbody.as_slice()])
                .collect(),
            Stmt::Match(node) => node.cases.iter().map(|c| c.body.as_slice()).collect(),
            _ => Vec::new(),
        }
    }

    /// Rebuild a compound statement with each nested suite mapped by `f`.
    pub(crate) fn map_child_suites(stmt: &Stmt, f: &dyn Fn(&[Stmt]) -> Vec<Stmt>) -> Stmt {
        let mut stmt = stmt.clone();
        match &mut stmt {
            Stmt::If(node) => {
                node.body = f(&node.body).into();
                for clause in &mut node.elif_else_clauses {
                    clause.body = f(&clause.body).into();
                }
            }
            Stmt::For(node) => {
                node.body = f(&node.body).into();
                node.orelse = f(&node.orelse).into();
            }
            Stmt::While(node) => {
                node.body = f(&node.body).into();
                node.orelse = f(&node.orelse).into();
            }
            Stmt::With(node) => node.body = f(&node.body).into(),
            Stmt::Try(node) => {
                node.body = f(&node.body).into();
                for handler in &mut node.handlers {
                    let ExceptHandler::ExceptHandler(handler) = handler;
                    handler.body = f(&handler.body).into();
                }
                node.orelse = f(&node.orelse).into();
                node.finalbody = f(&node.finalbody).into();
            }
            Stmt::Match(node) => {
                for case in &mut node.cases {
                    case.body = f(&case.body).into();
                }
            }
            _ => {}
        }
        stmt
    }

    /// Replace each nested `def`/`class` (also inside compound statements)
    /// with a name-only placeholder.
    pub(crate) fn placeholder_defs(stmts: &[Stmt]) -> Vec<Stmt> {
        stmts
            .iter()
            .map(|stmt| match stmt {
                Stmt::FunctionDef(def) => Stmt::FunctionDef(ast::StmtFunctionDef {
                    node_index: Default::default(),
                    range: TextRange::default(),
                    is_async: def.is_async,
                    decorator_list: Default::default(),
                    name: def.name.clone(),
                    type_params: None,
                    parameters: Box::default(),
                    returns: None,
                    body: Self::pass_body(),
                }),
                Stmt::ClassDef(class) => Stmt::ClassDef(ast::StmtClassDef {
                    node_index: Default::default(),
                    range: TextRange::default(),
                    decorator_list: Default::default(),
                    name: class.name.clone(),
                    type_params: None,
                    arguments: None,
                    body: Self::pass_body(),
                }),
                other => Self::map_child_suites(other, &Self::placeholder_defs),
            })
            .collect()
    }

    /// Drop each `def`/`class` (also inside compound statements): the
    /// module unit covers only the other top-level code.
    pub(crate) fn strip_defs(stmts: &[Stmt]) -> Vec<Stmt> {
        stmts
            .iter()
            .filter(|stmt| !matches!(stmt, Stmt::FunctionDef(_) | Stmt::ClassDef(_)))
            .map(|stmt| Self::map_child_suites(stmt, &Self::strip_defs))
            .collect()
    }

    fn pass_body<B: From<Vec<Stmt>>>() -> B {
        B::from(vec![Stmt::Pass(ast::StmtPass {
            node_index: Default::default(),
            range: TextRange::default(),
        })])
    }

    /// A fixed algorithm with fixed keys: checksums must be stable across
    /// runs and processes (never `std`'s randomly-keyed default hasher).
    pub fn hasher() -> SipHasher13 {
        SipHasher13::new_with_keys(0x7079_7465_7374_2d72, 0x732d_6166_6665_6374)
    }

    fn hash_stmt(stmt: &Stmt) -> u64 {
        let mut hasher = Self::hasher();
        ComparableStmt::from(stmt).hash(&mut hasher);
        hasher.finish()
    }

    fn hash_suite(stmts: &[Stmt]) -> u64 {
        let mut hasher = Self::hasher();
        stmts.len().hash(&mut hasher);
        for stmt in stmts {
            ComparableStmt::from(stmt).hash(&mut hasher);
        }
        hasher.finish()
    }

    /// Content hash of a whole file (the `<file>` pseudo unit, change
    /// detection).
    pub fn content_hash(bytes: &[u8]) -> u64 {
        let mut hasher = Self::hasher();
        hasher.write(bytes);
        hasher.finish()
    }

    /// Map a code object's `co_qualname` to the unit it belongs to: lambdas,
    /// comprehensions, annotation scopes and other synthesized scopes
    /// (`<...>` segments, `__annotate__`) belong to the enclosing unit;
    /// module-level ones to the module unit. A name that matches no unit of
    /// the file maps to the whole-file pseudo unit.
    pub fn unit_for_qualname(&self, co_qualname: &str) -> String {
        let mut segments: Vec<&str> = co_qualname.split('.').collect();
        while let Some(last) = segments.last() {
            if last.starts_with('<') || *last == "__annotate__" {
                segments.pop();
            } else {
                break;
            }
        }
        if segments.is_empty() {
            return MODULE_UNIT.to_string();
        }
        let qualname = segments.join(".");
        if self.units.contains_key(&qualname) {
            qualname
        } else {
            FILE_UNIT.to_string()
        }
    }

    /// The units a dependency on `qualname` implies: every enclosing
    /// function or class unit and the module unit.
    pub fn enclosing(qualname: &str) -> Vec<String> {
        if qualname == MODULE_UNIT || qualname == FILE_UNIT {
            return Vec::new();
        }
        let segments: Vec<&str> = qualname.split('.').collect();
        let mut out: Vec<String> = (1..segments.len())
            .map(|n| segments[..n].join("."))
            .filter(|prefix| !prefix.ends_with("<locals>"))
            .collect();
        out.push(MODULE_UNIT.to_string());
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The function, class and module units (the name units of
    /// `crate::names`, `scope:name`, are tested there).
    fn units(source: &str) -> HashMap<String, u64> {
        FileUnits::parse(source)
            .expect("parses")
            .units
            .into_iter()
            .filter(|(qualname, _)| !qualname.contains(':'))
            .collect()
    }

    /// The qualnames whose checksum differs between two versions (added and
    /// removed units included).
    fn changed(before: &str, after: &str) -> Vec<String> {
        let (a, b) = (units(before), units(after));
        let mut names: Vec<String> = a
            .keys()
            .chain(b.keys())
            .filter(|name| a.get(*name) != b.get(*name))
            .cloned()
            .collect();
        names.sort();
        names.dedup();
        names
    }

    const BASE: &str = r#"
import os

LIMIT = 3

def helper(x):
    return x + 1

@decorate
def decorated(a, b=2):
    """Doc."""
    def inner():
        return a
    return inner()

class Widget(Base):
    size = 1

    def method(self):
        return self.size

    class Inner:
        flag = True
"#;

    #[test]
    fn qualnames() {
        let mut names: Vec<String> = units(BASE).into_keys().collect();
        names.sort();
        assert_eq!(
            names,
            [
                "<module>",
                "Widget",
                "Widget.Inner",
                "Widget.method",
                "decorated",
                "decorated.<locals>.inner",
                "helper",
            ]
        );
    }

    #[test]
    fn formatting_and_comments_change_nothing() {
        let reformatted = r#"
import os
# a comment
LIMIT = (3)


def helper( x ):
    # another
    return (x + 1)

@decorate
def decorated(a, b = 2):
    '''Doc.'''
    def inner():  # trailing
        return a
    return inner()
class Widget( Base ):
    size = 1
    def method(self):
        return self.size
    class Inner:
        flag = True
"#;
        assert_eq!(changed(BASE, reformatted), Vec::<String>::new());
    }

    #[test]
    fn inserting_or_reordering_functions_changes_nothing() {
        let inserted = BASE.replace(
            "def helper(x):",
            "def brand_new():\n    return 0\n\ndef helper(x):",
        );
        assert_eq!(changed(BASE, &inserted), ["brand_new"]);
        let reordered = r#"
import os

LIMIT = 3

@decorate
def decorated(a, b=2):
    """Doc."""
    def inner():
        return a
    return inner()

def helper(x):
    return x + 1

class Widget(Base):
    size = 1

    def method(self):
        return self.size

    class Inner:
        flag = True
"#;
        assert_eq!(changed(BASE, reordered), Vec::<String>::new());
    }

    #[test]
    fn body_edit_changes_exactly_that_unit() {
        let edited = BASE.replace("return x + 1", "return x + 2");
        assert_eq!(changed(BASE, &edited), ["helper"]);
        let method = BASE.replace("return self.size", "return self.size * 2");
        assert_eq!(changed(BASE, &method), ["Widget.method"]);
    }

    #[test]
    fn decorator_and_signature_edits_change_exactly_that_unit() {
        let decorator = BASE.replace("@decorate\n", "@decorate(1)\n");
        assert_eq!(changed(BASE, &decorator), ["decorated"]);
        let default = BASE.replace("b=2", "b=3");
        assert_eq!(changed(BASE, &default), ["decorated"]);
    }

    #[test]
    fn nested_def_edit_changes_only_the_nested_unit() {
        let edited = BASE.replace("        return a\n", "        return a * 2\n");
        assert_eq!(changed(BASE, &edited), ["decorated.<locals>.inner"]);
    }

    #[test]
    fn class_attribute_edit_changes_exactly_the_class_unit() {
        let edited = BASE.replace("size = 1", "size = 2");
        assert_eq!(changed(BASE, &edited), ["Widget"]);
        let inner = BASE.replace("flag = True", "flag = False");
        assert_eq!(changed(BASE, &inner), ["Widget.Inner"]);
    }

    #[test]
    fn adding_a_method_changes_the_class_unit() {
        let edited = BASE.replace(
            "    def method(self):",
            "    def other(self):\n        return 0\n\n    def method(self):",
        );
        assert_eq!(changed(BASE, &edited), ["Widget", "Widget.other"]);
    }

    #[test]
    fn module_statement_edit_changes_the_module_unit() {
        let edited = BASE.replace("LIMIT = 3", "LIMIT = 4");
        assert_eq!(changed(BASE, &edited), ["<module>"]);
    }

    #[test]
    fn docstrings_count() {
        let edited = BASE.replace("\"\"\"Doc.\"\"\"", "\"\"\"Other doc.\"\"\"");
        assert_eq!(changed(BASE, &edited), ["decorated"]);
    }

    #[test]
    fn duplicate_definitions_merge() {
        let source =
            "if X:\n    def f():\n        return 1\nelse:\n    def f():\n        return 2\n";
        let edited = source.replace("return 2", "return 3");
        assert_eq!(units(source).len(), 2);
        assert_eq!(changed(source, &edited), ["f"]);
    }

    #[test]
    fn defs_inside_compound_statements_are_units() {
        let source = "try:\n    def f():\n        return 1\nexcept ImportError:\n    f = None\n";
        let edited = source.replace("return 1", "return 2");
        assert_eq!(changed(source, &edited), ["f"]);
    }

    #[test]
    fn unparseable_source_is_none() {
        assert!(FileUnits::parse("def f(:\n").is_none());
    }

    #[test]
    fn qualname_mapping() {
        let file = FileUnits::parse(BASE).expect("parses");
        assert_eq!(file.unit_for_qualname("<module>"), MODULE_UNIT);
        assert_eq!(file.unit_for_qualname("helper"), "helper");
        assert_eq!(file.unit_for_qualname("Widget"), "Widget");
        assert_eq!(file.unit_for_qualname("Widget.method"), "Widget.method");
        assert_eq!(
            file.unit_for_qualname("decorated.<locals>.inner"),
            "decorated.<locals>.inner"
        );
        assert_eq!(
            file.unit_for_qualname("decorated.<locals>.<lambda>"),
            "decorated"
        );
        assert_eq!(file.unit_for_qualname("<lambda>"), MODULE_UNIT);
        assert_eq!(file.unit_for_qualname("Widget.<genexpr>"), "Widget");
        assert_eq!(file.unit_for_qualname("helper.__annotate__"), "helper");
        assert_eq!(file.unit_for_qualname("generated_at_runtime"), FILE_UNIT);
    }

    #[test]
    fn enclosing_units() {
        assert_eq!(
            FileUnits::enclosing("Widget.Inner.go"),
            ["Widget", "Widget.Inner", "<module>"]
        );
        assert_eq!(
            FileUnits::enclosing("decorated.<locals>.inner"),
            ["decorated", "<module>"]
        );
        assert_eq!(FileUnits::enclosing("helper"), ["<module>"]);
        assert!(FileUnits::enclosing(MODULE_UNIT).is_empty());
    }

    #[test]
    fn checksums_are_stable() {
        // A fixed algorithm: the same source always hashes the same, in any
        // process (no per-process random keys).
        let first = units("def f():\n    return 1\n")["f"];
        let again = units("def f():\n    return 1\n")["f"];
        assert_eq!(first, again);
        assert_eq!(
            FileUnits::content_hash(b"abc"),
            FileUnits::content_hash(b"abc")
        );
    }
}
