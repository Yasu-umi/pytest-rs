//! Static direct imports of a test file (docs/affected.md, "When code
//! runs"): a module already in `sys.modules` when the test file was imported
//! ran none of its code then, so the file's own import statements name it
//! instead.

use ruff_python_ast::{self as ast, ExceptHandler, Stmt};

pub struct StaticImports;

impl StaticImports {
    /// The absolute module names the top-level `import` / `from ... import`
    /// statements of a module can bind, parents included (`import a.b`
    /// also runs `a`); for `from m import x`, `m.x` too in case `x` is a
    /// submodule. Statements nested in `if` / `try` / `with` blocks count
    /// (`if TYPE_CHECKING:` included: the over-run is harmless), function
    /// and class bodies do not. `module` is the importing module's dotted
    /// name, needed for relative imports; `is_package` when it is an
    /// `__init__.py`.
    pub fn of(source: &str, module: Option<&str>, is_package: bool) -> Vec<String> {
        let Ok(parsed) = ruff_python_parser::parse_module(source) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        Self::walk(parsed.suite(), module, is_package, &mut out);
        out.sort();
        out.dedup();
        out
    }

    fn walk(stmts: &[Stmt], module: Option<&str>, is_package: bool, out: &mut Vec<String>) {
        for stmt in stmts {
            match stmt {
                Stmt::Import(node) => {
                    for alias in &node.names {
                        out.extend(Self::with_parents(alias.name.id.as_str()));
                    }
                }
                Stmt::ImportFrom(node) => {
                    let Some(base) = Self::resolve_from(node, module, is_package) else {
                        continue;
                    };
                    out.extend(Self::with_parents(&base));
                    for alias in &node.names {
                        if alias.name.id.as_str() != "*" {
                            out.push(format!("{base}.{}", alias.name.id));
                        }
                    }
                }
                Stmt::If(node) => {
                    Self::walk(&node.body, module, is_package, out);
                    for clause in &node.elif_else_clauses {
                        Self::walk(&clause.body, module, is_package, out);
                    }
                }
                Stmt::Try(node) => {
                    Self::walk(&node.body, module, is_package, out);
                    for handler in &node.handlers {
                        let ExceptHandler::ExceptHandler(handler) = handler;
                        Self::walk(&handler.body, module, is_package, out);
                    }
                    Self::walk(&node.orelse, module, is_package, out);
                    Self::walk(&node.finalbody, module, is_package, out);
                }
                Stmt::With(node) => Self::walk(&node.body, module, is_package, out),
                _ => {}
            }
        }
    }

    fn with_parents(name: &str) -> Vec<String> {
        let parts: Vec<&str> = name.split('.').collect();
        (1..=parts.len()).map(|n| parts[..n].join(".")).collect()
    }

    fn resolve_from(
        node: &ast::StmtImportFrom,
        module: Option<&str>,
        is_package: bool,
    ) -> Option<String> {
        let target = node.module.as_ref().map(|m| m.id.to_string());
        if node.level == 0 {
            return target;
        }
        let mut parts: Vec<&str> = module?.split('.').collect();
        if !is_package {
            parts.pop();
        }
        for _ in 1..node.level {
            parts.pop()?;
        }
        let mut base = parts.join(".");
        if let Some(target) = target {
            if !base.is_empty() {
                base.push('.');
            }
            base.push_str(&target);
        }
        (!base.is_empty()).then_some(base)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absolute_and_relative_imports() {
        let source = r#"
import os.path
from app.core import add, Box
from . import helpers
from ..shared import util
try:
    import fast
except ImportError:
    fast = None
if TYPE_CHECKING:
    from app import types

def f():
    import inside_function
"#;
        let names = StaticImports::of(source, Some("tests.unit.test_x"), false);
        for expected in [
            "os",
            "os.path",
            "app",
            "app.core",
            "app.core.add",
            "app.core.Box",
            "tests.unit",
            "tests.unit.helpers",
            "tests.shared",
            "tests.shared.util",
            "fast",
            "app.types",
        ] {
            assert!(
                names.contains(&expected.to_string()),
                "{expected} in {names:?}"
            );
        }
        assert!(!names.iter().any(|n| n == "inside_function"));
    }

    #[test]
    fn relative_import_without_a_package_is_skipped() {
        assert!(StaticImports::of("from . import x\n", Some("test_x"), false).is_empty());
    }
}
