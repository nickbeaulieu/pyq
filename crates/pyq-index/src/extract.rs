//! One parse per file → a [`FileIndex`] of definitions and references.
//!
//! Built on `ruff_python_parser` + the `ruff_python_ast` visitor. Parse errors
//! are non-fatal: we extract what parsed and move on (a half-broken file an
//! agent is mid-edit on should still answer queries).

use ruff_python_ast::visitor::{walk_expr, walk_stmt, Visitor};
use ruff_python_ast::{CmpOp, Expr, ExprContext, PySourceType, Stmt, StmtClassDef};
use ruff_python_parser::parse_unchecked_source;
use ruff_text_size::{Ranged, TextSize};

use crate::model::{
    Def, DefKind, FileIndex, ImportContext, ImportStmt, Input, InputKind, Pos, Ref,
};

/// Parse `source` and extract its facts. `path` is recorded verbatim for output.
///
/// Uses ruff's error-recovering parser and walks the best-effort tree even when
/// the source has syntax errors — a file an agent is mid-edit on still answers
/// for every statement that parsed *before* the error (this is load-bearing for
/// the "half-edited file still answers" guarantee, not just a nicety).
pub fn extract(path: &str, source: &str) -> FileIndex {
    let source_type = if path.ends_with(".pyi") {
        PySourceType::Stub
    } else {
        PySourceType::Python
    };
    let parsed = parse_unchecked_source(source, source_type);
    let lines = Lines::new(source);
    let mut collector = Collector {
        source,
        lines: &lines,
        depth: 0,
        func_depth: 0,
        type_checking: false,
        defs: Vec::new(),
        refs: Vec::new(),
        inputs: Vec::new(),
        imports: Vec::new(),
    };
    for stmt in &parsed.syntax().body {
        collector.visit_stmt(stmt);
    }
    FileIndex {
        path: path.to_string(),
        defs: collector.defs,
        refs: collector.refs,
        inputs: collector.inputs,
        imports: collector.imports,
    }
}

struct Collector<'src> {
    source: &'src str,
    lines: &'src Lines,
    /// Function + class nesting (for `Def::nested`).
    depth: usize,
    /// Function-body nesting only — a non-zero depth means an import is deferred.
    func_depth: usize,
    /// Inside an `if TYPE_CHECKING:` block — its imports are type-only.
    type_checking: bool,
    defs: Vec<Def>,
    refs: Vec<Ref>,
    inputs: Vec<Input>,
    imports: Vec<ImportStmt>,
}

impl<'src> Collector<'src> {
    fn pos(&self, offset: TextSize) -> Pos {
        self.lines.pos(offset.to_usize(), self.source)
    }

    fn push_input(&mut self, kind: InputKind, value: String, offset: TextSize) {
        self.inputs.push(Input {
            kind,
            value,
            pos: self.pos(offset),
        });
    }

    /// Detect env-var reads and literal file opens. Syntactic and
    /// over-approximate: computed keys/paths become `<dynamic>` or are skipped.
    ///
    /// Env matching is suffix-based so it follows the common aliases — bare
    /// `getenv(...)`/`o.getenv(...)` (`import os as o`), and `environ[...]` /
    /// `environ.get(...)` (`from os import environ`) — not just `os.*`.
    fn collect_input(&mut self, expr: &Expr) {
        match expr {
            Expr::Call(call) => {
                let Some(callee) = dotted_name(&call.func) else {
                    return;
                };
                let first = call.arguments.args.first();
                // env reads: getenv(...), environ.get(...), and
                // environ.setdefault(...) (a read-with-fallback like .get).
                if is_getenv(&callee)
                    || callee.ends_with("environ.get")
                    || callee.ends_with("environ.setdefault")
                {
                    let value = literal_str(first).unwrap_or_else(|| "<dynamic>".into());
                    self.push_input(InputKind::Env, value, expr.range().start());
                    return;
                }
                match callee.as_str() {
                    "open" | "io.open" => {
                        if let Some(path) = literal_str(first) {
                            self.push_input(InputKind::File, path, expr.range().start());
                        }
                    }
                    // argparse `parser.add_argument("--flag")` and click
                    // `@click.option("--flag")` / `@click.argument("name")`.
                    _ if callee.ends_with(".add_argument")
                        || callee.ends_with(".option")
                        || callee.ends_with(".argument") =>
                    {
                        if let Some(name) = literal_str(first) {
                            self.push_input(InputKind::Arg, name, expr.range().start());
                        }
                    }
                    _ => {}
                }
            }
            Expr::Subscript(sub) if is_environ(dotted_name(&sub.value).as_deref()) => {
                let value = match sub.slice.as_ref() {
                    Expr::StringLiteral(s) => s.value.to_str().to_string(),
                    _ => "<dynamic>".into(),
                };
                self.push_input(InputKind::Env, value, expr.range().start());
            }
            // Membership tests: `"KEY" in os.environ` / `"KEY" not in os.environ`.
            Expr::Compare(cmp)
                if matches!(cmp.ops.first(), Some(CmpOp::In | CmpOp::NotIn))
                    && is_environ(cmp.comparators.first().and_then(dotted_name).as_deref()) =>
            {
                let value = match cmp.left.as_ref() {
                    Expr::StringLiteral(s) => s.value.to_str().to_string(),
                    _ => "<dynamic>".into(),
                };
                self.push_input(InputKind::Env, value, expr.range().start());
            }
            _ => {}
        }
    }

    /// When the import currently being visited executes.
    fn import_context(&self) -> ImportContext {
        if self.type_checking {
            ImportContext::TypeChecking
        } else if self.func_depth > 0 {
            ImportContext::Deferred
        } else {
            ImportContext::TopLevel
        }
    }

    fn push_def(&mut self, name: &str, kind: DefKind, offset: TextSize) {
        self.defs.push(Def {
            name: name.to_string(),
            kind,
            pos: self.pos(offset),
            nested: self.depth > 0,
        });
    }
}

impl<'src, 'ast> Visitor<'ast> for Collector<'src> {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        match stmt {
            Stmt::FunctionDef(f) => {
                self.push_def(f.name.as_str(), DefKind::Function, f.name.start());
                self.depth += 1;
                self.func_depth += 1;
                walk_stmt(self, stmt);
                self.func_depth -= 1;
                self.depth -= 1;
                return;
            }
            // `if TYPE_CHECKING:` — its body's imports are type-only and never
            // run at import time, so they must not count as cycle edges. The
            // elif/else branches are runtime and visited normally.
            Stmt::If(if_stmt) if is_type_checking(&if_stmt.test) => {
                self.visit_expr(&if_stmt.test);
                let prev = self.type_checking;
                self.type_checking = true;
                for s in &if_stmt.body {
                    self.visit_stmt(s);
                }
                self.type_checking = prev;
                for clause in &if_stmt.elif_else_clauses {
                    if let Some(test) = &clause.test {
                        self.visit_expr(test);
                    }
                    for s in &clause.body {
                        self.visit_stmt(s);
                    }
                }
                return;
            }
            Stmt::ClassDef(c) => {
                self.push_def(c.name.as_str(), DefKind::Class, c.name.start());
                // A pydantic BaseSettings subclass: its annotated class-level
                // fields are configuration inputs.
                if class_bases(c).any(|b| b.ends_with("BaseSettings")) {
                    for member in &c.body {
                        if let Stmt::AnnAssign(ann) = member {
                            if let Expr::Name(target) = ann.target.as_ref() {
                                self.push_input(
                                    InputKind::Setting,
                                    target.id.as_str().to_string(),
                                    target.start(),
                                );
                            }
                        }
                    }
                }
                self.depth += 1;
                walk_stmt(self, stmt);
                self.depth -= 1;
                return;
            }
            Stmt::Assign(a) => {
                for target in &a.targets {
                    if let Expr::Name(n) = target {
                        self.push_def(n.id.as_str(), DefKind::Variable, n.start());
                    }
                }
                // `env = os.environ` binds the whole mapping; the keys read
                // through it later are unknown, so flag the dependency.
                if matches!(a.value.as_ref(), Expr::Attribute(_) | Expr::Name(_))
                    && is_environ(dotted_name(&a.value).as_deref())
                {
                    self.push_input(InputKind::Env, "<dynamic>".into(), a.value.range().start());
                }
            }
            Stmt::Import(i) => {
                for alias in &i.names {
                    let bound = alias.asname.as_ref().unwrap_or(&alias.name);
                    self.push_def(bound.as_str(), DefKind::Import, bound.start());
                    // `import a.b` is an edge to the module `a.b` (level 0).
                    self.imports.push(ImportStmt {
                        module: alias.name.as_str().to_string(),
                        level: 0,
                        names: Vec::new(),
                        context: self.import_context(),
                        pos: self.pos(i.start()),
                    });
                }
            }
            Stmt::ImportFrom(i) => {
                for alias in &i.names {
                    let bound = alias.asname.as_ref().unwrap_or(&alias.name);
                    self.push_def(bound.as_str(), DefKind::Import, bound.start());
                }
                // `from <module> import a, b` is one edge to `<module>`; `names`
                // lets the graph resolve `from . import sub` into submodule edges.
                self.imports.push(ImportStmt {
                    module: i.module.as_ref().map(|m| m.as_str().to_string()).unwrap_or_default(),
                    level: i.level,
                    names: i.names.iter().map(|a| a.name.as_str().to_string()).collect(),
                    context: self.import_context(),
                    pos: self.pos(i.start()),
                });
            }
            _ => {}
        }
        walk_stmt(self, stmt);
    }

    fn visit_expr(&mut self, expr: &'ast Expr) {
        self.collect_input(expr);

        // A call's callee, when a bare name, is recorded as a call reference and
        // we skip re-walking it as a plain load (so `f` in `f()` is one ref).
        if let Expr::Call(call) = expr {
            if let Expr::Name(n) = call.func.as_ref() {
                self.refs.push(Ref {
                    name: n.id.as_str().to_string(),
                    pos: self.pos(n.start()),
                    is_call: true,
                });
                for arg in call.arguments.args.iter() {
                    self.visit_expr(arg);
                }
                for kw in call.arguments.keywords.iter() {
                    self.visit_expr(&kw.value);
                }
                return;
            }
        }
        if let Expr::Name(n) = expr {
            if matches!(n.ctx, ExprContext::Load) {
                self.refs.push(Ref {
                    name: n.id.as_str().to_string(),
                    pos: self.pos(n.start()),
                    is_call: false,
                });
            }
        }
        walk_expr(self, expr);
    }
}

/// Whether an `if` test is `TYPE_CHECKING` (bare or `typing.TYPE_CHECKING`).
fn is_type_checking(test: &Expr) -> bool {
    matches!(dotted_name(test).as_deref(), Some(d) if d == "TYPE_CHECKING" || d.ends_with(".TYPE_CHECKING"))
}

/// A `getenv` callee, following the `import os as o` / `from os import getenv`
/// aliases: bare `getenv` or any `*.getenv`.
fn is_getenv(callee: &str) -> bool {
    callee == "getenv" || callee.ends_with(".getenv")
}

/// Whether a dotted name refers to `environ`, following `from os import environ`
/// (bare `environ`) and `import os`/`import os as o` (`*.environ`).
fn is_environ(dotted: Option<&str>) -> bool {
    matches!(dotted, Some(d) if d == "environ" || d.ends_with(".environ"))
}

/// The dotted path of an attribute/name chain (`os.environ.get`), or `None`
/// for anything more complex (subscripts, calls in the middle, etc.).
fn dotted_name(expr: &Expr) -> Option<String> {
    match expr {
        Expr::Name(n) => Some(n.id.as_str().to_string()),
        Expr::Attribute(a) => Some(format!("{}.{}", dotted_name(&a.value)?, a.attr.as_str())),
        _ => None,
    }
}

/// Dotted names of a class's base classes.
fn class_bases(c: &StmtClassDef) -> impl Iterator<Item = String> + '_ {
    c.arguments
        .iter()
        .flat_map(|a| a.args.iter())
        .filter_map(dotted_name)
}

/// The value of a string-literal argument, if the expr is one.
fn literal_str(expr: Option<&Expr>) -> Option<String> {
    match expr? {
        Expr::StringLiteral(s) => Some(s.value.to_str().to_string()),
        _ => None,
    }
}

/// Precomputed line-start byte offsets → 1-based line/char-column conversion.
struct Lines {
    starts: Vec<usize>,
}

impl Lines {
    fn new(s: &str) -> Self {
        let mut starts = vec![0usize];
        for (i, b) in s.bytes().enumerate() {
            if b == b'\n' {
                starts.push(i + 1);
            }
        }
        Lines { starts }
    }

    fn pos(&self, byte: usize, s: &str) -> Pos {
        let line = self.starts.partition_point(|&st| st <= byte);
        let line_start = self.starts[line - 1];
        let col = s[line_start..byte].chars().count() + 1;
        Pos { line, col }
    }
}
