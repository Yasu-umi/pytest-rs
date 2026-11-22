//! Name-level dependencies on import-time code: `import_time = "names"`
//! (docs/affected.md).
//!
//! Two halves:
//! - [`ModuleFacts::analyze`] splits a file's module-level and class-level
//!   statements into one unit per bound name, and lists what each of those
//!   units and each function references. It needs nothing but the file, so
//!   the units' checksums are part of every file's units whatever the
//!   setting.
//! - [`Narrower`] resolves those references across the project at the end
//!   of a recording session, and replaces a test's dependencies on whole
//!   module and class units with the units its code can reach by name.
//!
//! Every rule errs towards more dependencies: what cannot be followed falls
//! back to the whole module (or, for an attribute of an object of unknown
//! type, to every attribute of that name).

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::rc::Rc;

use ruff_python_ast::comparable::ComparableStmt;
use ruff_python_ast::visitor::{self, Visitor};
use ruff_python_ast::{
    self as ast, Comprehension, ExceptHandler, Expr, ExprContext, Pattern, Stmt,
};

use crate::imports::StaticImports;
use crate::units::{FILE_UNIT, FileUnits, MODULE_UNIT, Unit};

/// The module-level statements that bind no name.
pub const EFFECTS: &str = "<effects>";
/// The first element of a [`Ref::Chain`] that starts in a class body's
/// scope: `[CLASS_SCOPE, class qualname, name, attributes...]`.
pub const CLASS_SCOPE: &str = "<class-scope>";
/// A class's header: decorators, bases, keywords, the names of its methods
/// and nested classes, and the statements binding dunder names or none.
pub const CLASS_HEADER: &str = "<class>";

/// What a piece of code references.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Ref {
    /// A name looked up in the module scope (or in a class body's scope,
    /// see [`CLASS_SCOPE`]), then attributes: `nx.utils.f`.
    Chain(Vec<String>),
    /// An attribute of a value that is not followed (`self.x`, `g.nodes`).
    Attr(String),
    /// `from m import x` (absolute `m`).
    ModuleAttr(String, String),
    /// An attribute whose name is computed (`getattr(o, name)`,
    /// `vars(o)`, `o.__dict__`), of the chain if the object is one.
    Dynamic(Option<Vec<String>>),
    /// `globals()`, or `vars()`/`locals()` outside a function.
    Globals,
}

/// What a group of module-level or class-level statements references, the
/// callees it calls (None: not a name chain), and whether it writes
/// anything but plain names.
#[derive(Debug, Clone, Default)]
pub struct Facts {
    pub refs: Vec<Ref>,
    pub callees: Vec<Option<Vec<String>>>,
    pub impure: bool,
}

impl Facts {
    fn absorb(&mut self, other: &Facts) {
        self.refs.extend(other.refs.iter().cloned());
        self.callees.extend(other.callees.iter().cloned());
        self.impure |= other.impure;
    }
}

/// How a module-level name is bound.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Binding {
    /// `import a.b` binds `a` to module `a`; `import a.b as c`, `c` to `a.b`.
    Module(String),
    /// `from m import x` (absolute `m`).
    From(String, String),
    Function,
    /// A class, by qualname.
    Class(String),
    Assigned,
}

#[derive(Debug, Clone, Default)]
pub struct ClassFacts {
    /// Name chains of the bases (`Generic[T]` counts as `Generic`); None
    /// for anything else.
    pub bases: Vec<Option<Vec<String>>>,
    pub metaclass: Option<Option<Vec<String>>>,
    /// Keywords other than `metaclass`.
    pub keywords: bool,
    /// The callee chain of each decorator.
    pub decorators: Vec<Option<Vec<String>>>,
    pub init_subclass: bool,
    pub header: Facts,
    /// Dunder names bound in the body (part of the header).
    pub header_names: BTreeSet<String>,
    /// Other names bound in the body by statements (not methods).
    pub attrs: BTreeMap<String, Facts>,
    /// Every statement of the body other than `def`/`class`.
    pub body: Facts,
}

#[derive(Debug, Clone, Default)]
pub struct ModuleFacts {
    pub bindings: HashMap<String, Vec<Binding>>,
    /// `<module>:NAME` units.
    pub names: BTreeMap<String, Facts>,
    pub effects: Facts,
    /// Star imports, in order (None: a relative import that cannot be
    /// resolved).
    pub stars: Vec<Option<String>>,
    /// `__all__` when it is one literal list or tuple of strings.
    pub all: Option<Vec<String>>,
    /// `__all__` is bound some other way.
    pub dynamic_all: bool,
    /// Classes outside functions, by qualname.
    pub classes: BTreeMap<String, ClassFacts>,
    /// Functions and classes inside functions, by qualname.
    pub code: HashMap<String, Vec<Ref>>,
}

impl ModuleFacts {
    /// Analyze a parsed module. `module` (its dotted name) and `is_package`
    /// resolve relative imports; without them the facts are incomplete but
    /// the units are the same.
    pub fn analyze(suite: &[Stmt], module: Option<&str>, is_package: bool) -> (Self, Vec<Unit>) {
        let mut analyzer = Analyzer {
            module,
            is_package,
            facts: ModuleFacts::default(),
            units: Vec::new(),
        };
        analyzer.module_body(suite);
        (analyzer.facts, analyzer.units)
    }

    pub fn is_dunder(name: &str) -> bool {
        name.len() > 4 && name.starts_with("__") && name.ends_with("__")
    }

    pub fn unit_name(scope: &str, name: &str) -> String {
        format!("{scope}:{name}")
    }
}

struct Analyzer<'m> {
    module: Option<&'m str>,
    is_package: bool,
    facts: ModuleFacts,
    units: Vec<Unit>,
}

impl<'m> Analyzer<'m> {
    fn collector(&self, level: Level, scopes: Vec<HashSet<String>>) -> RefCollector<'m> {
        RefCollector {
            level,
            module: self.module,
            is_package: self.is_package,
            scopes,
            class: None,
            facts: Facts::default(),
        }
    }

    fn binder(&self) -> Binder<'m> {
        Binder {
            module: self.module,
            is_package: self.is_package,
            prefix: String::new(),
            out: Vec::new(),
            stars: Vec::new(),
            globals: HashSet::new(),
            comprehension: 0,
        }
    }

    fn bind(&mut self, name: &str, binding: Binding) {
        let entry = self.facts.bindings.entry(name.to_string()).or_default();
        if !entry.contains(&binding) {
            entry.push(binding);
        }
    }

    fn is_docstring(stmt: &Stmt) -> bool {
        matches!(stmt, Stmt::Expr(e) if e.value.is_string_literal_expr())
    }

    /// A statement's checksum: the statement (nested `def`/`class`
    /// stripped) and, for each name it reads from its scope, how many
    /// earlier statements of that scope bound it, so that moving a
    /// definition across a use changes the user.
    fn checksum(stmt: &Stmt, positional: &BTreeSet<(String, usize)>) -> u64 {
        let mut hasher = FileUnits::hasher();
        ComparableStmt::from(stmt).hash(&mut hasher);
        positional.hash(&mut hasher);
        hasher.finish()
    }

    fn positional(
        refs: &[Ref],
        counts: &HashMap<String, usize>,
        class: Option<&str>,
    ) -> BTreeSet<(String, usize)> {
        refs.iter()
            .filter_map(|r| match (r, class) {
                (Ref::Chain(chain), None) if chain[0] != CLASS_SCOPE => Some(chain[0].clone()),
                (Ref::Chain(chain), Some(class))
                    if chain[0] == CLASS_SCOPE && chain[1] == class =>
                {
                    Some(chain[2].clone())
                }
                _ => None,
            })
            .map(|name| {
                let count = counts.get(&name).copied().unwrap_or(0);
                (name, count)
            })
            .collect()
    }

    fn module_body(&mut self, suite: &[Stmt]) {
        let mut counts: HashMap<String, usize> = HashMap::new();
        for (index, stmt) in suite.iter().enumerate() {
            match stmt {
                Stmt::FunctionDef(def) => {
                    self.bind(def.name.as_str(), Binding::Function);
                    *counts.entry(def.name.to_string()).or_default() += 1;
                    self.function(def, def.name.as_str(), &[]);
                }
                Stmt::ClassDef(class) => {
                    let name = class.name.to_string();
                    self.bind(&name, Binding::Class(name.clone()));
                    *counts.entry(name.clone()).or_default() += 1;
                    self.class(class, &name, None);
                }
                other => {
                    let doc = index == 0 && Self::is_docstring(other);
                    self.module_stmt(other, doc, &mut counts);
                }
            }
        }
    }

    /// `def`/`class` nested in compound statements of a module or class
    /// body (`if X: def f(): ...`): units of their own, as at the top.
    fn nested_defs(&mut self, stmt: &Stmt, prefix: &str) {
        for suite in FileUnits::child_suites(stmt) {
            for inner in suite {
                match inner {
                    Stmt::FunctionDef(def) => {
                        self.function(def, &format!("{prefix}{}", def.name), &[]);
                    }
                    Stmt::ClassDef(class) => {
                        self.class(class, &format!("{prefix}{}", class.name), None);
                    }
                    other => self.nested_defs(other, prefix),
                }
            }
        }
    }

    fn module_stmt(&mut self, stmt: &Stmt, doc: bool, counts: &mut HashMap<String, usize>) {
        self.nested_defs(stmt, "");
        let mut binder = self.binder();
        binder.visit_stmt(stmt);
        for (name, binding) in &binder.out {
            self.bind(name, binding.clone());
        }
        self.facts.stars.extend(binder.stars.iter().cloned());
        let mut collector = self.collector(Level::Module, Vec::new());
        collector.visit_stmt(stmt);
        let facts = collector.facts;

        let mut names: Vec<String> = Vec::new();
        for (name, _) in &binder.out {
            if !names.contains(name) {
                names.push(name.clone());
            }
        }
        if doc {
            names = vec!["__doc__".to_string()];
        }
        if names.iter().any(|n| n == "__all__") {
            self.facts.all = match (stmt, self.facts.bindings.get("__all__").map(Vec::len)) {
                (Stmt::Assign(assign), Some(1))
                    if self.facts.all.is_none() && !self.facts.dynamic_all =>
                {
                    Self::literal_strings(&assign.value)
                }
                _ => None,
            };
            self.facts.dynamic_all = self.facts.all.is_none();
        }
        let positional = Self::positional(&facts.refs, counts, None);
        let stripped = FileUnits::map_child_suites(stmt, &FileUnits::strip_defs);
        let checksum = Self::checksum(&stripped, &positional);
        if names.is_empty() {
            self.units.push(Unit {
                qualname: ModuleFacts::unit_name(MODULE_UNIT, EFFECTS),
                checksum,
            });
            self.facts.effects.absorb(&facts);
        }
        for name in &names {
            self.units.push(Unit {
                qualname: ModuleFacts::unit_name(MODULE_UNIT, name),
                checksum,
            });
            self.facts
                .names
                .entry(name.clone())
                .or_default()
                .absorb(&facts);
            *counts.entry(name.clone()).or_default() += 1;
        }
    }

    fn literal_strings(expr: &Expr) -> Option<Vec<String>> {
        let elts = match expr {
            Expr::List(list) => &list.elts,
            Expr::Tuple(tuple) => &tuple.elts,
            _ => return None,
        };
        elts.iter()
            .map(|e| match e {
                Expr::StringLiteral(s) => Some(s.value.to_str().to_string()),
                _ => None,
            })
            .collect()
    }

    fn parameter_names(parameters: &ast::Parameters) -> HashSet<String> {
        parameters.iter().map(|p| p.name().to_string()).collect()
    }

    /// A function unit's references: its decorators, defaults and
    /// annotations (in the enclosing scopes) and its body.
    fn function(
        &mut self,
        def: &ast::StmtFunctionDef,
        qualname: &str,
        enclosing: &[HashSet<String>],
    ) {
        let mut outer = self.collector(Level::Function, enclosing.to_vec());
        for decorator in &def.decorator_list {
            outer.visit_decorator(decorator);
        }
        outer.visit_parameters(&def.parameters);
        if let Some(returns) = &def.returns {
            outer.visit_annotation(returns);
        }

        let mut locals = Self::parameter_names(&def.parameters);
        let mut binder = self.binder();
        binder.visit_body(&def.body);
        locals.extend(binder.out.into_iter().map(|(name, _)| name));
        for global in &binder.globals {
            locals.remove(global);
        }
        let mut scopes = enclosing.to_vec();
        scopes.push(locals);
        let mut inner = self.collector(Level::Function, scopes.clone());
        inner.visit_body(&def.body);

        let entry = self.facts.code.entry(qualname.to_string()).or_default();
        entry.extend(outer.facts.refs);
        entry.extend(inner.facts.refs);
        self.nested_in_function(&def.body, qualname, &scopes);
    }

    fn nested_in_function(&mut self, body: &[Stmt], qualname: &str, scopes: &[HashSet<String>]) {
        for stmt in body {
            match stmt {
                Stmt::FunctionDef(def) => {
                    self.function(def, &format!("{qualname}.<locals>.{}", def.name), scopes);
                }
                Stmt::ClassDef(class) => {
                    self.local_class(
                        class,
                        &format!("{qualname}.<locals>.{}", class.name),
                        scopes,
                    );
                }
                other => {
                    for suite in FileUnits::child_suites(other) {
                        self.nested_in_function(suite, qualname, scopes);
                    }
                }
            }
        }
    }

    /// A class inside a function: a whole unit, its references in one list.
    fn local_class(
        &mut self,
        class: &ast::StmtClassDef,
        qualname: &str,
        scopes: &[HashSet<String>],
    ) {
        let mut binder = self.binder();
        binder.visit_body(&class.body);
        let names: HashSet<String> = binder.out.into_iter().map(|(name, _)| name).collect();
        let mut collector = self.collector(Level::Function, scopes.to_vec());
        for decorator in &class.decorator_list {
            collector.visit_decorator(decorator);
        }
        if let Some(arguments) = &class.arguments {
            collector.visit_arguments(arguments);
        }
        collector.scopes.push(names);
        collector.visit_body(&class.body);
        self.facts
            .code
            .entry(qualname.to_string())
            .or_default()
            .extend(collector.facts.refs);
        for stmt in &class.body {
            match stmt {
                Stmt::FunctionDef(def) => {
                    self.function(def, &format!("{qualname}.{}", def.name), scopes);
                }
                Stmt::ClassDef(inner) => {
                    self.local_class(inner, &format!("{qualname}.{}", inner.name), scopes);
                }
                _ => {}
            }
        }
    }

    fn decorator_callee(
        collector: &RefCollector,
        decorator: &ast::Decorator,
    ) -> Option<Vec<String>> {
        match &decorator.expression {
            Expr::Call(call) => collector.chain(&call.func),
            other => collector.chain(other),
        }
    }

    fn base_chain(collector: &RefCollector, base: &Expr) -> Option<Vec<String>> {
        match base {
            Expr::Subscript(subscript) => collector.chain(&subscript.value),
            other => collector.chain(other),
        }
    }

    /// A class outside functions: one unit per name its body binds, and
    /// its header.
    /// `outer`: the class whose body this class is defined in, and the
    /// names that body binds (its header is evaluated in that scope).
    fn class(
        &mut self,
        class: &ast::StmtClassDef,
        qualname: &str,
        outer: Option<(String, HashSet<String>)>,
    ) {
        let mut facts = ClassFacts::default();
        let mut header = self.collector(Level::Module, Vec::new());
        header.class = outer;
        for decorator in &class.decorator_list {
            facts
                .decorators
                .push(Self::decorator_callee(&header, decorator));
            header.visit_decorator(decorator);
        }
        if let Some(arguments) = &class.arguments {
            for base in &arguments.args {
                facts.bases.push(Self::base_chain(&header, base));
            }
            for keyword in &arguments.keywords {
                match keyword.arg.as_ref().map(|a| a.as_str()) {
                    Some("metaclass") => facts.metaclass = Some(header.chain(&keyword.value)),
                    _ => facts.keywords = true,
                }
            }
            header.visit_arguments(arguments);
        }
        facts.header = header.facts;

        let mut binder = self.binder();
        binder.visit_body(&class.body);
        let class_names: HashSet<String> = binder.out.into_iter().map(|(name, _)| name).collect();

        let prefix = format!("{qualname}.");
        let mut counts: HashMap<String, usize> = HashMap::new();
        let mut header_body: Vec<Stmt> = Vec::new();
        let mut header_positional: BTreeSet<(String, usize)> = BTreeSet::new();
        for (index, stmt) in class.body.iter().enumerate() {
            match stmt {
                Stmt::FunctionDef(def) => {
                    if def.name.as_str() == "__init_subclass__" {
                        facts.init_subclass = true;
                    }
                    header_body.extend(FileUnits::placeholder_defs(std::slice::from_ref(stmt)));
                    *counts.entry(def.name.to_string()).or_default() += 1;
                    self.function(def, &format!("{prefix}{}", def.name), &[]);
                }
                Stmt::ClassDef(inner) => {
                    header_body.extend(FileUnits::placeholder_defs(std::slice::from_ref(stmt)));
                    *counts.entry(inner.name.to_string()).or_default() += 1;
                    self.class(
                        inner,
                        &format!("{prefix}{}", inner.name),
                        Some((qualname.to_string(), class_names.clone())),
                    );
                }
                other => {
                    self.nested_defs(other, &prefix);
                    let mut binder = self.binder();
                    binder.visit_stmt(other);
                    let mut collector = self.collector(Level::Class, Vec::new());
                    collector.class = Some((qualname.to_string(), class_names.clone()));
                    collector.visit_stmt(other);
                    let stmt_facts = collector.facts;
                    facts.body.absorb(&stmt_facts);
                    let positional = Self::positional(&stmt_facts.refs, &counts, Some(qualname));
                    let stripped = FileUnits::map_child_suites(other, &FileUnits::strip_defs);
                    let mut names: Vec<String> = Vec::new();
                    for (name, _) in &binder.out {
                        if !names.contains(name) {
                            names.push(name.clone());
                        }
                    }
                    let dunders: Vec<&String> =
                        names.iter().filter(|n| ModuleFacts::is_dunder(n)).collect();
                    let plain: Vec<&String> = names
                        .iter()
                        .filter(|n| !ModuleFacts::is_dunder(n))
                        .collect();
                    let doc = index == 0 && Self::is_docstring(other);
                    if doc || plain.is_empty() {
                        header_body.push(stripped.clone());
                        header_positional.extend(positional.iter().cloned());
                        facts.header.absorb(&stmt_facts);
                        facts
                            .header_names
                            .extend(dunders.iter().map(|n| (*n).clone()));
                    }
                    if !doc {
                        let checksum = Self::checksum(&stripped, &positional);
                        for name in &plain {
                            self.units.push(Unit {
                                qualname: ModuleFacts::unit_name(qualname, name),
                                checksum,
                            });
                            facts
                                .attrs
                                .entry((*name).clone())
                                .or_default()
                                .absorb(&stmt_facts);
                        }
                    }
                    for name in names {
                        *counts.entry(name).or_default() += 1;
                    }
                }
            }
        }
        let mut stub = class.clone();
        stub.body = if header_body.is_empty() {
            vec![Stmt::Pass(ast::StmtPass {
                node_index: Default::default(),
                range: Default::default(),
            })]
            .into()
        } else {
            header_body.into()
        };
        self.units.push(Unit {
            qualname: ModuleFacts::unit_name(qualname, CLASS_HEADER),
            checksum: Self::checksum(&Stmt::ClassDef(stub), &header_positional),
        });
        self.facts.classes.insert(qualname.to_string(), facts);
    }
}

/// The names a statement (or a body) binds in its own scope, and how.
/// Does not look into nested functions, classes, lambdas or comprehensions
/// (except for `:=`, which binds in the enclosing scope).
struct Binder<'m> {
    module: Option<&'m str>,
    is_package: bool,
    prefix: String,
    out: Vec<(String, Binding)>,
    stars: Vec<Option<String>>,
    globals: HashSet<String>,
    comprehension: usize,
}

impl Binder<'_> {
    fn push(&mut self, name: &str, binding: Binding) {
        self.out.push((name.to_string(), binding));
    }
}

impl<'a> Visitor<'a> for Binder<'_> {
    fn visit_stmt(&mut self, stmt: &'a Stmt) {
        match stmt {
            Stmt::FunctionDef(def) => self.push(def.name.as_str(), Binding::Function),
            Stmt::ClassDef(class) => {
                let qualname = format!("{}{}", self.prefix, class.name);
                self.push(class.name.as_str(), Binding::Class(qualname));
            }
            Stmt::Import(node) => {
                for alias in &node.names {
                    let full = alias.name.as_str();
                    match &alias.asname {
                        Some(asname) => {
                            self.push(asname.as_str(), Binding::Module(full.to_string()))
                        }
                        None => {
                            let top = full.split('.').next().unwrap_or(full);
                            self.push(top, Binding::Module(top.to_string()));
                        }
                    }
                }
            }
            Stmt::ImportFrom(node) => {
                let base = StaticImports::resolve_from(node, self.module, self.is_package);
                for alias in &node.names {
                    let name = alias.name.as_str();
                    if name == "*" {
                        self.stars.push(base.clone());
                        continue;
                    }
                    let local = alias.asname.as_ref().map_or(name, |a| a.as_str());
                    let binding = match &base {
                        Some(base) => Binding::From(base.clone(), name.to_string()),
                        None => Binding::Assigned,
                    };
                    self.push(local, binding);
                }
            }
            Stmt::Global(node) => self
                .globals
                .extend(node.names.iter().map(|n| n.to_string())),
            Stmt::Nonlocal(node) => self
                .globals
                .extend(node.names.iter().map(|n| n.to_string())),
            _ => visitor::walk_stmt(self, stmt),
        }
    }

    fn visit_expr(&mut self, expr: &'a Expr) {
        match expr {
            Expr::Name(name) if matches!(name.ctx, ExprContext::Store | ExprContext::Del) => {
                if self.comprehension == 0 {
                    self.push(name.id.as_str(), Binding::Assigned);
                }
            }
            Expr::Named(named) => {
                if let Expr::Name(name) = named.target.as_ref() {
                    self.push(name.id.as_str(), Binding::Assigned);
                }
                self.visit_expr(&named.value);
            }
            Expr::Lambda(_) => {}
            Expr::ListComp(_) | Expr::SetComp(_) | Expr::DictComp(_) | Expr::Generator(_) => {
                self.comprehension += 1;
                visitor::walk_expr(self, expr);
                self.comprehension -= 1;
            }
            _ => visitor::walk_expr(self, expr),
        }
    }

    fn visit_except_handler(&mut self, handler: &'a ExceptHandler) {
        let ExceptHandler::ExceptHandler(inner) = handler;
        if let Some(name) = &inner.name {
            self.push(name.as_str(), Binding::Assigned);
        }
        visitor::walk_except_handler(self, handler);
    }

    fn visit_pattern(&mut self, pattern: &'a Pattern) {
        match pattern {
            Pattern::MatchAs(node) => {
                if let Some(name) = &node.name {
                    self.push(name.as_str(), Binding::Assigned);
                }
            }
            Pattern::MatchStar(node) => {
                if let Some(name) = &node.name {
                    self.push(name.as_str(), Binding::Assigned);
                }
            }
            Pattern::MatchMapping(node) => {
                if let Some(name) = &node.rest {
                    self.push(name.as_str(), Binding::Assigned);
                }
            }
            _ => {}
        }
        visitor::walk_pattern(self, pattern);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Level {
    Module,
    Class,
    Function,
}

/// Collects what code references: names it does not bind itself (looked up
/// in the module), attributes, computed attribute access, callees.
struct RefCollector<'m> {
    level: Level,
    module: Option<&'m str>,
    is_package: bool,
    /// Function, lambda and comprehension scopes, innermost last.
    scopes: Vec<HashSet<String>>,
    /// The class whose body this is, and the names the body binds.
    class: Option<(String, HashSet<String>)>,
    facts: Facts,
}

impl RefCollector<'_> {
    fn bound(&self, name: &str) -> bool {
        self.scopes.iter().any(|scope| scope.contains(name))
    }

    /// The name chain an expression is (`a.b.c`), starting in the class
    /// body's scope or the module's, if it is one.
    fn chain(&self, expr: &Expr) -> Option<Vec<String>> {
        match expr {
            Expr::Name(name) if !self.bound(&name.id) => Some(match &self.class {
                Some((qualname, names)) if names.contains(name.id.as_str()) => vec![
                    CLASS_SCOPE.to_string(),
                    qualname.clone(),
                    name.id.to_string(),
                ],
                _ => vec![name.id.to_string()],
            }),
            Expr::Attribute(attribute) => {
                let mut chain = self.chain(&attribute.value)?;
                chain.push(attribute.attr.to_string());
                Some(chain)
            }
            _ => None,
        }
    }

    fn add_chain(&mut self, chain: Vec<String>) {
        match chain.iter().position(|part| part == "__dict__") {
            Some(index) => {
                self.facts
                    .refs
                    .push(Ref::Dynamic((index > 0).then(|| chain[..index].to_vec())));
                for part in &chain[index + 1..] {
                    self.facts.refs.push(Ref::Attr(part.clone()));
                }
            }
            None => self.facts.refs.push(Ref::Chain(chain)),
        }
    }

    fn target_names(expr: &Expr, out: &mut HashSet<String>) {
        match expr {
            Expr::Name(name) => {
                out.insert(name.id.to_string());
            }
            Expr::Tuple(tuple) => tuple.elts.iter().for_each(|e| Self::target_names(e, out)),
            Expr::List(list) => list.elts.iter().for_each(|e| Self::target_names(e, out)),
            Expr::Starred(starred) => Self::target_names(&starred.value, out),
            _ => {}
        }
    }

    fn comprehension(&mut self, generators: &[Comprehension], elements: &[&Expr]) {
        // The first iterable is evaluated in the enclosing scope.
        if let Some(first) = generators.first() {
            self.visit_expr(&first.iter);
        }
        let mut names = HashSet::new();
        for generator in generators {
            Self::target_names(&generator.target, &mut names);
        }
        self.scopes.push(names);
        for (index, generator) in generators.iter().enumerate() {
            if index > 0 {
                self.visit_expr(&generator.iter);
            }
            for condition in &generator.ifs {
                self.visit_expr(condition);
            }
        }
        for element in elements {
            self.visit_expr(element);
        }
        self.scopes.pop();
    }

    fn call(&mut self, call: &ast::ExprCall) {
        self.facts.callees.push(self.chain(&call.func));
        let Expr::Name(func) = call.func.as_ref() else {
            return;
        };
        if self.bound(&func.id) {
            return;
        }
        let args = &call.arguments.args;
        match func.id.as_str() {
            "getattr" | "hasattr" | "setattr" | "delattr" if args.len() >= 2 => {
                if matches!(func.id.as_str(), "setattr" | "delattr")
                    && self.level != Level::Function
                {
                    self.facts.impure = true;
                }
                match &args[1] {
                    Expr::StringLiteral(literal) => {
                        let attr = literal.value.to_str().to_string();
                        match self.chain(&args[0]) {
                            Some(mut chain) => {
                                chain.push(attr);
                                self.add_chain(chain);
                            }
                            None => self.facts.refs.push(Ref::Attr(attr)),
                        }
                    }
                    _ => {
                        let chain = self.chain(&args[0]);
                        self.facts.refs.push(Ref::Dynamic(chain));
                    }
                }
            }
            "globals" if args.is_empty() => self.facts.refs.push(Ref::Globals),
            "vars" | "locals" if args.is_empty() && self.level != Level::Function => {
                self.facts.refs.push(Ref::Globals);
            }
            "vars" if args.len() == 1 => {
                let chain = self.chain(&args[0]);
                self.facts.refs.push(Ref::Dynamic(chain));
            }
            _ => {}
        }
    }
}

impl<'a> Visitor<'a> for RefCollector<'_> {
    fn visit_stmt(&mut self, stmt: &'a Stmt) {
        match stmt {
            // A nested definition's decorators, defaults and bases are
            // evaluated here; its body is a unit of its own.
            Stmt::FunctionDef(def) => {
                for decorator in &def.decorator_list {
                    self.visit_decorator(decorator);
                }
                self.visit_parameters(&def.parameters);
                if let Some(returns) = &def.returns {
                    self.visit_annotation(returns);
                }
            }
            Stmt::ClassDef(class) => {
                for decorator in &class.decorator_list {
                    self.visit_decorator(decorator);
                }
                if let Some(arguments) = &class.arguments {
                    self.visit_arguments(arguments);
                }
            }
            Stmt::ImportFrom(node) => {
                if let Some(base) = StaticImports::resolve_from(node, self.module, self.is_package)
                {
                    for alias in &node.names {
                        if alias.name.as_str() != "*" {
                            self.facts
                                .refs
                                .push(Ref::ModuleAttr(base.clone(), alias.name.to_string()));
                        }
                    }
                }
            }
            _ => visitor::walk_stmt(self, stmt),
        }
    }

    fn visit_expr(&mut self, expr: &'a Expr) {
        match expr {
            Expr::Name(name) => {
                if name.ctx == ExprContext::Load
                    && let Some(chain) = self.chain(expr)
                {
                    self.facts.refs.push(Ref::Chain(chain));
                }
            }
            Expr::Attribute(attribute) => {
                if matches!(attribute.ctx, ExprContext::Store | ExprContext::Del)
                    && self.level != Level::Function
                {
                    self.facts.impure = true;
                }
                match self.chain(expr) {
                    Some(chain) => self.add_chain(chain),
                    None => {
                        let mut base = expr;
                        while let Expr::Attribute(inner) = base {
                            if inner.attr.as_str() == "__dict__" {
                                self.facts.refs.push(Ref::Dynamic(None));
                            } else {
                                self.facts.refs.push(Ref::Attr(inner.attr.to_string()));
                            }
                            base = &inner.value;
                        }
                        self.visit_expr(base);
                    }
                }
            }
            Expr::Subscript(subscript) => {
                if matches!(subscript.ctx, ExprContext::Store | ExprContext::Del)
                    && self.level != Level::Function
                {
                    self.facts.impure = true;
                }
                visitor::walk_expr(self, expr);
            }
            Expr::Call(call) => {
                self.call(call);
                visitor::walk_expr(self, expr);
            }
            Expr::Lambda(lambda) => {
                let mut names = HashSet::new();
                if let Some(parameters) = &lambda.parameters {
                    for parameter in parameters.iter_non_variadic_params() {
                        if let Some(default) = &parameter.default {
                            self.visit_expr(default);
                        }
                    }
                    names = Analyzer::parameter_names(parameters);
                }
                self.scopes.push(names);
                self.visit_expr(&lambda.body);
                self.scopes.pop();
            }
            Expr::ListComp(node) => self.comprehension(&node.generators, &[&node.elt]),
            Expr::SetComp(node) => self.comprehension(&node.generators, &[&node.elt]),
            Expr::Generator(node) => self.comprehension(&node.generators, &[&node.elt]),
            Expr::DictComp(node) => {
                let mut elements: Vec<&Expr> = Vec::new();
                if let Some(key) = &node.key {
                    elements.push(key);
                }
                elements.push(&node.value);
                self.comprehension(&node.generators, &elements);
            }
            _ => visitor::walk_expr(self, expr),
        }
    }
}

/// How the session's modules map to files.
#[derive(Debug, Clone)]
pub enum ModuleKind {
    /// A project module, by rootdir-relative path.
    Project(String),
    /// A project namespace package (no file).
    Namespace,
    External {
        third_party: bool,
    },
}

#[derive(Debug, Clone, Default)]
pub struct ModuleMap {
    pub modules: HashMap<String, ModuleKind>,
    /// Module name of each project file.
    pub names: HashMap<String, String>,
}

/// What a name evaluates to, as far as it is followed.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Value {
    Module(String),
    Class(u32, String),
    External { third_party: bool },
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Node {
    /// A unit, output as a dependency (when the file has it).
    Unit(u32, String),
    /// A function (or a class inside one) a test ran: its references.
    Code(u32, String),
    /// A whole module: its module unit and every class, and everything
    /// they reference.
    Module(u32),
    /// Every unit binding this attribute name in the modules the test
    /// reached.
    Attr(String),
    /// A computed attribute of an object of unknown type: every class the
    /// test touches, whole (a module object obtained at run time is not
    /// followed, docs/affected.md "Known gaps").
    Dynamic,
}

#[derive(Debug, Clone, Copy)]
struct ClassKind {
    /// Its attributes are units of their own.
    narrowable: bool,
    /// Defining it can change other state (a decorator or metaclass that is
    /// not the standard library's, a base whose `__init_subclass__` runs):
    /// a dependency of every test that reaches its module.
    effectful: bool,
    /// Defining a subclass runs code of this class.
    hooks: bool,
}

pub struct FileInfo {
    pub facts: ModuleFacts,
    pub units: HashMap<String, u64>,
}

/// Replaces a test's dependencies on whole module and class units with
/// the name units its code reaches.
pub struct Narrower<'a> {
    map: ModuleMap,
    builtins: HashSet<String>,
    /// Source of a rootdir-relative file.
    load: &'a dyn Fn(&str) -> Option<String>,
    /// Whether a file's import-time code may be narrowed (a project module,
    /// not a test file, a conftest, or `always_changed`).
    narrowable: &'a dyn Fn(&str) -> bool,
    files: Vec<String>,
    file_ids: HashMap<String, u32>,
    infos: Vec<Option<Option<Rc<FileInfo>>>>,
    nodes: Vec<Node>,
    node_ids: HashMap<Node, u32>,
    edges: Vec<Option<Rc<[u32]>>>,
    kinds: HashMap<(u32, String), ClassKind>,
    always: HashMap<u32, Rc<[u32]>>,
    attr_index: HashMap<u32, Rc<HashMap<String, Vec<u32>>>>,
    /// Attribute name -> the indexed files binding it, and their nodes.
    attr_files: HashMap<String, Vec<(u32, Vec<u32>)>>,
    finding: HashSet<(u32, String)>,
}

impl<'a> Narrower<'a> {
    const MODULE_DUNDERS: &'static [&'static str] = &[
        "__name__",
        "__file__",
        "__doc__",
        "__spec__",
        "__loader__",
        "__package__",
        "__path__",
        "__builtins__",
        "__cached__",
        "__annotations__",
        "__dict__",
    ];

    pub fn new(
        map: ModuleMap,
        builtins: HashSet<String>,
        load: &'a dyn Fn(&str) -> Option<String>,
        narrowable: &'a dyn Fn(&str) -> bool,
    ) -> Self {
        Self {
            map,
            builtins,
            load,
            narrowable,
            files: Vec::new(),
            file_ids: HashMap::new(),
            infos: Vec::new(),
            nodes: Vec::new(),
            node_ids: HashMap::new(),
            edges: Vec::new(),
            kinds: HashMap::new(),
            always: HashMap::new(),
            attr_index: HashMap::new(),
            attr_files: HashMap::new(),
            finding: HashSet::new(),
        }
    }

    fn file_id(&mut self, rel: &str) -> u32 {
        if let Some(&id) = self.file_ids.get(rel) {
            return id;
        }
        let id = self.files.len() as u32;
        self.files.push(rel.to_string());
        self.file_ids.insert(rel.to_string(), id);
        self.infos.push(None);
        id
    }

    pub fn info(&mut self, rel: &str) -> Option<Rc<FileInfo>> {
        let id = self.file_id(rel);
        self.file(id)
    }

    fn file(&mut self, id: u32) -> Option<Rc<FileInfo>> {
        if let Some(done) = &self.infos[id as usize] {
            return done.clone();
        }
        let rel = self.files[id as usize].clone();
        let info = (self.load)(&rel).and_then(|source| {
            let parsed = ruff_python_parser::parse_module(&source).ok()?;
            let module = self.map.names.get(&rel).map(String::as_str);
            let (facts, _) =
                ModuleFacts::analyze(parsed.suite(), module, rel.ends_with("__init__.py"));
            let units = FileUnits::from_suite(parsed.suite()).units;
            Some(Rc::new(FileInfo { facts, units }))
        });
        self.infos[id as usize] = Some(info.clone());
        info
    }

    fn node(&mut self, node: Node) -> u32 {
        if let Some(&id) = self.node_ids.get(&node) {
            return id;
        }
        let id = self.nodes.len() as u32;
        self.nodes.push(node.clone());
        self.node_ids.insert(node, id);
        self.edges.push(None);
        id
    }

    fn project_file(&mut self, module: &str) -> Option<u32> {
        match self.map.modules.get(module) {
            Some(ModuleKind::Project(rel)) => {
                let rel = rel.clone();
                Some(self.file_id(&rel))
            }
            _ => None,
        }
    }

    fn class_node(&mut self, file: u32, qualname: &str) -> u32 {
        let unit = if self.kind(file, qualname).narrowable {
            ModuleFacts::unit_name(qualname, CLASS_HEADER)
        } else {
            qualname.to_string()
        };
        self.node(Node::Unit(file, unit))
    }

    /// What `name` is bound to in `file`, following star imports; None when
    /// the module binds no such name.
    fn find(&mut self, file: u32, name: &str, out: &mut Vec<u32>) -> Option<Value> {
        let info = self.file(file)?;
        let key = (file, name.to_string());
        if !self.finding.insert(key.clone()) {
            // A cycle of imports: the outer lookup already added its units.
            return Some(Value::Unknown);
        }
        let found = self.find_inner(file, &info, name, out);
        self.finding.remove(&key);
        found
    }

    fn find_inner(
        &mut self,
        file: u32,
        info: &Rc<FileInfo>,
        name: &str,
        out: &mut Vec<u32>,
    ) -> Option<Value> {
        if let Some(bindings) = info.facts.bindings.get(name) {
            if info.facts.names.contains_key(name) {
                let unit = ModuleFacts::unit_name(MODULE_UNIT, name);
                out.push(self.node(Node::Unit(file, unit)));
            }
            let mut value: Option<Value> = None;
            let mut mixed = false;
            for binding in bindings.clone() {
                let this = match binding {
                    Binding::Module(module) => Value::Module(module),
                    Binding::From(module, attr) => self.attr(Value::Module(module), &attr, out),
                    Binding::Function | Binding::Assigned => Value::Unknown,
                    Binding::Class(qualname) => {
                        let node = self.class_node(file, &qualname);
                        out.push(node);
                        Value::Class(file, qualname)
                    }
                };
                match &value {
                    None => value = Some(this),
                    Some(previous) if *previous == this => {}
                    Some(_) => mixed = true,
                }
            }
            return Some(if mixed {
                Value::Unknown
            } else {
                value.unwrap_or(Value::Unknown)
            });
        }
        let mut external = None;
        for star in info.facts.stars.clone() {
            let module = star?;
            match self.map.modules.get(&module).cloned() {
                Some(ModuleKind::Project(rel)) => {
                    let source = self.file_id(&rel);
                    let Some(source_info) = self.file(source) else {
                        continue;
                    };
                    let exported = match &source_info.facts.all {
                        Some(all) => all.iter().any(|n| n == name),
                        None => source_info.facts.dynamic_all || !name.starts_with('_'),
                    };
                    if !exported {
                        continue;
                    }
                    if source_info.facts.names.contains_key("__all__") {
                        let unit = ModuleFacts::unit_name(MODULE_UNIT, "__all__");
                        out.push(self.node(Node::Unit(source, unit)));
                    }
                    if let Some(value) = self.find(source, name, out) {
                        return Some(value);
                    }
                }
                Some(ModuleKind::External { third_party }) => {
                    external.get_or_insert(third_party);
                }
                _ => {}
            }
        }
        external.map(|third_party| Value::External { third_party })
    }

    /// A name looked up in a module's scope. A name the module does not
    /// bind and that is not a builtin may be set some other way: the whole
    /// module.
    fn name(&mut self, file: u32, name: &str, out: &mut Vec<u32>) -> Value {
        if let Some(value) = self.find(file, name, out) {
            return value;
        }
        if self.builtins.contains(name) || Self::MODULE_DUNDERS.contains(&name) {
            return Value::External { third_party: false };
        }
        out.push(self.node(Node::Module(file)));
        Value::Unknown
    }

    fn attr(&mut self, value: Value, attr: &str, out: &mut Vec<u32>) -> Value {
        match value {
            Value::Module(module) => {
                let submodule = format!("{module}.{attr}");
                match self.map.modules.get(&module).cloned() {
                    Some(ModuleKind::Project(rel)) => {
                        let file = self.file_id(&rel);
                        if let Some(value) = self.find(file, attr, out) {
                            return value;
                        }
                        if self.map.modules.contains_key(&submodule) {
                            return Value::Module(submodule);
                        }
                        if Self::MODULE_DUNDERS.contains(&attr) {
                            return Value::External { third_party: false };
                        }
                        out.push(self.node(Node::Module(file)));
                        Value::Unknown
                    }
                    Some(ModuleKind::Namespace) => {
                        if self.map.modules.contains_key(&submodule) {
                            Value::Module(submodule)
                        } else {
                            out.push(self.node(Node::Attr(attr.to_string())));
                            Value::Unknown
                        }
                    }
                    Some(ModuleKind::External { third_party }) => {
                        if matches!(
                            self.map.modules.get(&submodule),
                            Some(ModuleKind::Project(_))
                        ) {
                            Value::Module(submodule)
                        } else {
                            Value::External { third_party }
                        }
                    }
                    // Never imported: nothing it binds can have run.
                    None => Value::External { third_party: false },
                }
            }
            Value::External { third_party } => Value::External { third_party },
            Value::Class(..) | Value::Unknown => {
                out.push(self.node(Node::Attr(attr.to_string())));
                Value::Unknown
            }
        }
    }

    fn chain(&mut self, file: u32, chain: &[String], out: &mut Vec<u32>) -> Value {
        let (mut value, rest) = if chain[0] == CLASS_SCOPE {
            (
                self.class_scope(file, &chain[1], &chain[2], out),
                &chain[3..],
            )
        } else {
            (self.name(file, &chain[0], out), &chain[1..])
        };
        for attr in rest {
            value = self.attr(value, attr, out);
        }
        value
    }

    /// A name of a class body's scope: a nested class, or a class
    /// attribute (a dunder one is part of the header).
    fn class_scope(&mut self, file: u32, class: &str, name: &str, out: &mut Vec<u32>) -> Value {
        let nested = format!("{class}.{name}");
        let is_class = self
            .file(file)
            .is_some_and(|info| info.facts.classes.contains_key(&nested));
        if is_class {
            out.push(self.class_node(file, &nested));
            return Value::Class(file, nested);
        }
        let unit = if ModuleFacts::is_dunder(name) || !self.kind(file, class).narrowable {
            if self.kind(file, class).narrowable {
                ModuleFacts::unit_name(class, CLASS_HEADER)
            } else {
                class.to_string()
            }
        } else {
            ModuleFacts::unit_name(class, name)
        };
        out.push(self.node(Node::Unit(file, unit)));
        Value::Unknown
    }

    fn chain_value(&mut self, file: u32, chain: &[String]) -> Value {
        self.chain(file, chain, &mut Vec::new())
    }

    fn refs(&mut self, file: u32, refs: &[Ref], out: &mut Vec<u32>) {
        for reference in refs {
            match reference {
                Ref::Chain(chain) => {
                    self.chain(file, chain, out);
                }
                Ref::Attr(attr) => out.push(self.node(Node::Attr(attr.clone()))),
                Ref::ModuleAttr(module, attr) => {
                    self.attr(Value::Module(module.clone()), attr, out);
                }
                Ref::Dynamic(Some(chain)) => match self.chain(file, chain, out) {
                    Value::Module(module) => {
                        if let Some(target) = self.project_file(&module) {
                            out.push(self.node(Node::Module(target)));
                        }
                    }
                    Value::External { .. } => {}
                    _ => out.push(self.node(Node::Dynamic)),
                },
                Ref::Dynamic(None) => out.push(self.node(Node::Dynamic)),
                Ref::Globals => out.push(self.node(Node::Module(file))),
            }
        }
    }

    fn pure_callee(&mut self, file: u32, callee: &Option<Vec<String>>) -> bool {
        match callee {
            None => false,
            Some(chain) => matches!(self.chain_value(file, chain), Value::External { .. }),
        }
    }

    /// Statements that can change state other than the names they bind: a
    /// write to an attribute or an item, a computed attribute, a call to
    /// anything but the standard library and installed packages.
    fn effectful(&mut self, file: u32, facts: &Facts) -> bool {
        facts.impure
            || facts
                .refs
                .iter()
                .any(|r| matches!(r, Ref::Dynamic(_) | Ref::Globals))
            || facts
                .callees
                .iter()
                .any(|callee| !self.pure_callee(file, callee))
    }

    fn kind(&mut self, file: u32, qualname: &str) -> ClassKind {
        let key = (file, qualname.to_string());
        if let Some(kind) = self.kinds.get(&key) {
            return *kind;
        }
        // A class that is its own ancestor: as cautious as possible.
        self.kinds.insert(
            key.clone(),
            ClassKind {
                narrowable: false,
                effectful: true,
                hooks: true,
            },
        );
        let kind = self.kind_inner(file, qualname);
        self.kinds.insert(key, kind);
        kind
    }

    fn kind_inner(&mut self, file: u32, qualname: &str) -> ClassKind {
        let Some(info) = self.file(file) else {
            return ClassKind {
                narrowable: false,
                effectful: false,
                hooks: false,
            };
        };
        let Some(class) = info.facts.classes.get(qualname) else {
            return ClassKind {
                narrowable: false,
                effectful: false,
                hooks: false,
            };
        };
        let mut kind = ClassKind {
            narrowable: !class.init_subclass && !class.keywords && class.decorators.is_empty(),
            effectful: false,
            hooks: class.init_subclass,
        };
        for decorator in &class.decorators {
            if !self.pure_callee(file, decorator) {
                kind.effectful = true;
            }
        }
        if let Some(metaclass) = &class.metaclass {
            kind.narrowable = false;
            let standard = metaclass.as_ref().is_some_and(|chain| {
                matches!(
                    self.chain_value(file, chain),
                    Value::External { third_party: false }
                )
            });
            if !standard {
                kind.effectful = true;
                kind.hooks = true;
            }
        }
        for base in &class.bases {
            let Some(chain) = base else {
                kind.narrowable = false;
                kind.effectful = true;
                kind.hooks = true;
                continue;
            };
            match self.chain_value(file, chain) {
                Value::Class(base_file, base_qualname) => {
                    let base_kind = self.kind(base_file, &base_qualname);
                    if !base_kind.narrowable {
                        kind.narrowable = false;
                    }
                    if base_kind.hooks {
                        kind.effectful = true;
                        kind.hooks = true;
                    }
                }
                Value::External { third_party } => {
                    if !(chain.len() == 1 && chain[0] == "object") {
                        kind.narrowable = false;
                    }
                    if third_party {
                        kind.effectful = true;
                        kind.hooks = true;
                    }
                }
                _ => {
                    kind.narrowable = false;
                    kind.effectful = true;
                    kind.hooks = true;
                }
            }
        }
        if self.effectful(file, &class.body) {
            kind.effectful = true;
        }
        if kind.effectful {
            kind.narrowable = false;
        }
        kind
    }

    /// What every test reaching a module depends on: the statements that
    /// bind no name, those that can change other state, and the classes
    /// whose definition can.
    fn always(&mut self, file: u32) -> Rc<[u32]> {
        if let Some(done) = self.always.get(&file) {
            return done.clone();
        }
        let mut out = Vec::new();
        if let Some(info) = self.file(file) {
            let effects = ModuleFacts::unit_name(MODULE_UNIT, EFFECTS);
            if info.units.contains_key(&effects) {
                out.push(self.node(Node::Unit(file, effects)));
            }
            for (name, facts) in &info.facts.names {
                if self.effectful(file, facts) {
                    let unit = ModuleFacts::unit_name(MODULE_UNIT, name);
                    out.push(self.node(Node::Unit(file, unit)));
                }
            }
            for qualname in info.facts.classes.keys() {
                if self.kind(file, qualname).effectful {
                    out.push(self.node(Node::Unit(file, qualname.clone())));
                }
            }
        }
        let out: Rc<[u32]> = out.into();
        self.always.insert(file, out.clone());
        out
    }

    fn attr_index(&mut self, file: u32) -> Rc<HashMap<String, Vec<u32>>> {
        if let Some(done) = self.attr_index.get(&file) {
            return done.clone();
        }
        let mut index: HashMap<String, Vec<u32>> = HashMap::new();
        if let Some(info) = self.file(file) {
            for (qualname, class) in &info.facts.classes {
                let names = class.attrs.keys().chain(class.header_names.iter());
                if self.kind(file, qualname).narrowable {
                    for name in class.attrs.keys() {
                        let unit = ModuleFacts::unit_name(qualname, name);
                        let node = self.node(Node::Unit(file, unit));
                        index.entry(name.clone()).or_default().push(node);
                    }
                    for name in &class.header_names {
                        let node = self.class_node(file, qualname);
                        index.entry(name.clone()).or_default().push(node);
                    }
                } else {
                    let node = self.node(Node::Unit(file, qualname.clone()));
                    for name in names {
                        index.entry(name.clone()).or_default().push(node);
                    }
                }
                let last = qualname.rsplit('.').next().unwrap_or(qualname);
                let node = self.class_node(file, qualname);
                index.entry(last.to_string()).or_default().push(node);
            }
            for name in info.facts.names.keys() {
                let unit = ModuleFacts::unit_name(MODULE_UNIT, name);
                let node = self.node(Node::Unit(file, unit));
                index.entry(name.clone()).or_default().push(node);
            }
        }
        for (name, nodes) in &index {
            self.attr_files
                .entry(name.clone())
                .or_default()
                .push((file, nodes.clone()));
        }
        let index = Rc::new(index);
        self.attr_index.insert(file, index.clone());
        index
    }

    /// The nodes a node leads to (Attr and Dynamic depend on the test, and
    /// are expanded by the caller).
    fn edges(&mut self, id: u32) -> Rc<[u32]> {
        if let Some(done) = &self.edges[id as usize] {
            return done.clone();
        }
        let mut out = Vec::new();
        match self.nodes[id as usize].clone() {
            Node::Unit(file, unit) => {
                if let Some(info) = self.file(file) {
                    let facts = &info.facts;
                    if let Some(name) = unit.strip_prefix("<module>:") {
                        let refs = if name == EFFECTS {
                            Some(&facts.effects)
                        } else {
                            facts.names.get(name)
                        };
                        if let Some(f) = refs {
                            self.refs(file, &f.refs, &mut out);
                        }
                    } else if let Some(class) = facts.classes.get(&unit) {
                        self.refs(file, &class.header.refs, &mut out);
                        for attr in class.attrs.values() {
                            self.refs(file, &attr.refs, &mut out);
                        }
                    } else if let Some((qualname, name)) = unit.rsplit_once(':')
                        && let Some(class) = facts.classes.get(qualname)
                    {
                        if name == CLASS_HEADER {
                            self.refs(file, &class.header.refs, &mut out);
                        } else if let Some(attr) = class.attrs.get(name) {
                            self.refs(file, &attr.refs, &mut out);
                        }
                    }
                }
            }
            Node::Code(file, qualname) => {
                if let Some(info) = self.file(file) {
                    if let Some(refs) = info.facts.code.get(&qualname) {
                        self.refs(file, refs, &mut out);
                    }
                    // The classes it is a method of.
                    let segments: Vec<&str> = qualname.split('.').collect();
                    for n in 1..segments.len() {
                        let prefix = segments[..n].join(".");
                        if prefix.contains("<locals>") {
                            break;
                        }
                        if info.facts.classes.contains_key(&prefix) {
                            out.push(self.class_node(file, &prefix));
                        }
                    }
                }
            }
            Node::Module(file) => {
                out.push(self.node(Node::Unit(file, MODULE_UNIT.to_string())));
                if let Some(info) = self.file(file) {
                    for unit in info.units.keys() {
                        if unit.starts_with("<module>:") {
                            out.push(self.node(Node::Unit(file, unit.clone())));
                        }
                    }
                    for qualname in info.facts.classes.keys() {
                        out.push(self.node(Node::Unit(file, qualname.clone())));
                    }
                }
            }
            Node::Attr(_) | Node::Dynamic => {}
        }
        let out: Rc<[u32]> = out.into();
        self.edges[id as usize] = Some(out.clone());
        out
    }

    /// Whether a dependency is replaced by name units: the module unit or a
    /// class unit of a module whose import-time code may be narrowed.
    pub fn drops(&mut self, rel: &str, qualname: &str) -> bool {
        if !rel.ends_with(".py") || !(self.narrowable)(rel) || !self.map.names.contains_key(rel) {
            return false;
        }
        let file = self.file_id(rel);
        let Some(info) = self.file(file) else {
            return false;
        };
        qualname == MODULE_UNIT || info.facts.classes.contains_key(qualname)
    }

    /// Extend `closure` with more dependencies (rootdir-relative file,
    /// qualname): everything they reach by name.
    pub fn extend(&mut self, closure: &mut Closure, deps: &[(&str, &str)]) {
        let mut roots: Vec<u32> = Vec::new();
        let mut reached: Vec<u32> = Vec::new();
        for (rel, qualname) in deps {
            if !rel.ends_with(".py") || (qualname.starts_with('<') && *qualname != MODULE_UNIT) {
                continue;
            }
            let drops = self.drops(rel, qualname);
            let file = self.file_id(rel);
            let Some(info) = self.file(file) else {
                continue;
            };
            if *qualname == MODULE_UNIT {
                reached.push(file);
                if !drops {
                    roots.push(self.node(Node::Module(file)));
                }
            } else if info.facts.classes.contains_key(*qualname) {
                if !drops {
                    roots.push(self.node(Node::Unit(file, qualname.to_string())));
                }
            } else if *qualname != FILE_UNIT {
                roots.push(self.node(Node::Code(file, qualname.to_string())));
            }
        }
        for file in reached {
            if closure.reaches(file) {
                continue;
            }
            closure.reached.insert(file);
            self.attr_index(file);
            let rel = self.files[file as usize].clone();
            if (self.narrowable)(&rel) {
                roots.extend(self.always(file).iter().copied());
            }
            // Attributes already followed may also be bound in this module.
            let attrs: Vec<u32> = closure.attrs().collect();
            for attr in attrs {
                if let Node::Attr(name) = self.nodes[attr as usize].clone()
                    && let Some(nodes) = self.attr_index(file).get(&name)
                {
                    roots.extend(nodes.iter().copied());
                }
            }
        }

        let mut stack = roots;
        loop {
            while let Some(id) = stack.pop() {
                if closure.contains(id) {
                    continue;
                }
                closure.seen.insert(id);
                closure.order.push(id);
                match self.nodes[id as usize].clone() {
                    Node::Attr(attr) => {
                        closure.attrs.push(id);
                        if let Some(files) = self.attr_files.get(&attr) {
                            for (file, nodes) in files {
                                if closure.reaches(*file) {
                                    stack.extend(nodes.iter().copied());
                                }
                            }
                        }
                    }
                    Node::Dynamic => closure.dynamic = true,
                    Node::Unit(file, unit) => {
                        let class = unit.strip_suffix(":<class>").unwrap_or(&unit);
                        let is_class = self.infos[file as usize]
                            .as_ref()
                            .and_then(|info| info.as_ref())
                            .is_some_and(|info| info.facts.classes.contains_key(class));
                        if is_class {
                            closure.classes.push((file, class.to_string()));
                        }
                        stack.extend(self.edges(id).iter().copied());
                    }
                    _ => stack.extend(self.edges(id).iter().copied()),
                }
            }
            // A computed attribute of an object of unknown type: every
            // attribute of every class the test touched (named it, or ran
            // one of its methods).
            if !closure.is_dynamic() {
                break;
            }
            for (file, class) in closure.touched() {
                let id = self.node(Node::Unit(file, class));
                if !closure.contains(id) {
                    stack.push(id);
                }
            }
            if stack.is_empty() {
                break;
            }
        }
    }

    /// The units `closure` added over its base: (file, qualname, checksum).
    pub fn units(&mut self, closure: &Closure) -> Vec<(String, String, u64)> {
        let mut out: BTreeMap<(String, String), u64> = BTreeMap::new();
        for &id in &closure.order {
            if let Node::Unit(file, unit) = &self.nodes[id as usize] {
                let file = *file;
                let unit = unit.clone();
                if let Some(info) = self.file(file)
                    && let Some(&checksum) = info.units.get(&unit)
                {
                    out.insert((self.files[file as usize].clone(), unit), checksum);
                }
            }
        }
        out.into_iter()
            .map(|((rel, unit), checksum)| (rel, unit, checksum))
            .collect()
    }
}

/// What a set of dependencies reaches by name, so far: on top of a base
/// (the dependencies every test shares), which it does not repeat.
#[derive(Debug, Default)]
pub struct Closure {
    base: Option<Rc<Closure>>,
    seen: HashSet<u32>,
    /// `seen` in insertion order.
    order: Vec<u32>,
    /// Files whose module unit is among the dependencies.
    reached: HashSet<u32>,
    /// The `Node::Attr` nodes seen.
    attrs: Vec<u32>,
    dynamic: bool,
    /// Classes touched (a header or whole-class unit seen).
    classes: Vec<(u32, String)>,
}

impl Closure {
    pub fn on(base: Rc<Closure>) -> Self {
        Self {
            base: Some(base),
            ..Self::default()
        }
    }

    fn layers(&self) -> impl Iterator<Item = &Closure> {
        std::iter::successors(Some(self), |c| c.base.as_deref())
    }

    fn contains(&self, id: u32) -> bool {
        self.layers().any(|c| c.seen.contains(&id))
    }

    fn reaches(&self, file: u32) -> bool {
        self.layers().any(|c| c.reached.contains(&file))
    }

    fn attrs(&self) -> impl Iterator<Item = u32> + '_ {
        self.layers().flat_map(|c| c.attrs.iter().copied())
    }

    fn is_dynamic(&self) -> bool {
        self.layers().any(|c| c.dynamic)
    }

    /// The classes touched. A dynamic base already widened its own.
    fn touched(&self) -> Vec<(u32, String)> {
        let base_done = self.base.as_deref().is_some_and(Closure::is_dynamic);
        let layers: Vec<&Closure> = if base_done {
            vec![self]
        } else {
            self.layers().collect()
        };
        layers
            .into_iter()
            .flat_map(|layer| layer.classes.iter().cloned())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn units(source: &str) -> HashMap<String, u64> {
        FileUnits::parse(source).expect("parses").units
    }

    fn facts(source: &str) -> ModuleFacts {
        let parsed = ruff_python_parser::parse_module(source).expect("parses");
        ModuleFacts::analyze(parsed.suite(), Some("pkg.mod"), false).0
    }

    fn changed(before: &str, after: &str) -> BTreeSet<String> {
        let (a, b) = (units(before), units(after));
        a.keys()
            .chain(b.keys())
            .filter(|q| a.get(*q) != b.get(*q))
            .cloned()
            .collect()
    }

    fn set(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn one_unit_per_bound_name() {
        let before = "import os\nA = 1\nB = A + 1\nprint(A)\nclass C:\n    x = 1\n    y = 2\n";
        assert_eq!(
            changed(before, &before.replace("A = 1", "A = 2")),
            set(&["<module>", "<module>:A"])
        );
        assert_eq!(
            changed(before, &before.replace("print(A)", "print(B)")),
            set(&["<module>", "<module>:<effects>"])
        );
        assert_eq!(
            changed(before, &before.replace("x = 1", "x = 3")),
            set(&["C", "C:x"])
        );
        // Adding a name adds a unit and changes no other name unit.
        assert_eq!(
            changed(before, &before.replace("A = 1", "A = 1\nZ = 0")),
            set(&["<module>", "<module>:Z"])
        );
    }

    #[test]
    fn class_header_covers_bases_methods_and_dunders() {
        let before =
            "class C(object):\n    __slots__ = ()\n    x = 1\n    def f(self):\n        pass\n";
        assert_eq!(
            changed(before, &before.replace("(object)", "")),
            set(&["C", "C:<class>"])
        );
        assert_eq!(
            changed(
                before,
                &before.replace("__slots__ = ()", "__slots__ = ('a',)")
            ),
            set(&["C", "C:<class>"])
        );
        assert_eq!(
            changed(before, &before.replace("def f(self)", "def g(self)")),
            set(&["C", "C:<class>", "C.f", "C.g"])
        );
    }

    #[test]
    fn moving_a_definition_across_a_use_changes_the_user() {
        let before = "A = 1\nB = A\nA = 2\n";
        let after = "A = 1\nA = 2\nB = A\n";
        assert!(changed(before, after).contains("<module>:B"));
    }

    #[test]
    fn bindings_and_references() {
        let facts = facts(
            "import a.b\nimport c.d as e\nfrom .sib import x as y\nfrom q import *\n\
             def f(p):\n    local = p\n    return LIMIT + local + a.b.k + [i for i in p]\n",
        );
        assert_eq!(facts.bindings["a"], vec![Binding::Module("a".into())]);
        assert_eq!(facts.bindings["e"], vec![Binding::Module("c.d".into())]);
        assert_eq!(
            facts.bindings["y"],
            vec![Binding::From("pkg.sib".into(), "x".into())]
        );
        assert_eq!(facts.stars, vec![Some("q".to_string())]);
        let refs = &facts.code["f"];
        assert!(refs.contains(&Ref::Chain(vec!["LIMIT".into()])));
        assert!(refs.contains(&Ref::Chain(vec!["a".into(), "b".into(), "k".into()])));
        assert!(
            !refs.iter().any(
                |r| matches!(r, Ref::Chain(c) if c[0] == "local" || c[0] == "i" || c[0] == "p")
            )
        );
    }

    #[test]
    fn computed_attributes() {
        let facts = facts(
            "def f(o, n):\n    getattr(o, 'x')\n    getattr(M, n)\n    o.__dict__\n    globals()\n",
        );
        let refs = &facts.code["f"];
        assert!(refs.contains(&Ref::Attr("x".into())));
        assert!(refs.contains(&Ref::Dynamic(Some(vec!["M".into()]))));
        assert!(refs.contains(&Ref::Dynamic(None)));
        assert!(refs.contains(&Ref::Globals));
    }

    #[test]
    fn writes_and_calls_at_module_level() {
        let facts = facts("import m\nm.X = 1\nY = f(1)\nZ = [1]\n");
        assert!(facts.effects.impure);
        assert_eq!(facts.names["Y"].callees, vec![Some(vec!["f".to_string()])]);
        assert!(facts.names["Z"].callees.is_empty());
    }

    #[test]
    fn static_all() {
        assert_eq!(
            facts("__all__ = ['a', 'b']\n").all,
            Some(vec!["a".to_string(), "b".to_string()])
        );
        let dynamic = facts("__all__ = ['a']\n__all__ += ['b']\n");
        assert_eq!(dynamic.all, None);
        assert!(dynamic.dynamic_all);
    }

    #[test]
    fn nested_class_bases_resolve_in_the_outer_body() {
        let facts = facts("class A:\n    class B:\n        pass\n    class C(B):\n        pass\n");
        assert_eq!(
            facts.classes["A.C"].bases,
            vec![Some(vec![CLASS_SCOPE.to_string(), "A".into(), "B".into()])]
        );
    }
}
