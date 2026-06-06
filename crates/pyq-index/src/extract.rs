//! One parse per file → a [`FileIndex`] of definitions and references.
//!
//! Built on `ruff_python_parser` + the `ruff_python_ast` visitor. Parse errors
//! are non-fatal: we extract what parsed and move on (a half-broken file an
//! agent is mid-edit on should still answer queries).

use ruff_python_ast::visitor::{walk_expr, walk_stmt, Visitor};
use ruff_python_ast::{Expr, ExprContext, Stmt};
use ruff_text_size::{Ranged, TextSize};

use crate::model::{Def, DefKind, FileIndex, Input, InputKind, Pos, Ref};

/// Parse `source` and extract its facts. `path` is recorded verbatim for output.
pub fn extract(path: &str, source: &str) -> FileIndex {
    let parsed = ruff_python_parser::parse_module(source);
    let lines = Lines::new(source);
    let mut collector = Collector {
        source,
        lines: &lines,
        depth: 0,
        defs: Vec::new(),
        refs: Vec::new(),
        inputs: Vec::new(),
    };
    if let Ok(parsed) = parsed {
        for stmt in &parsed.syntax().body {
            collector.visit_stmt(stmt);
        }
    }
    FileIndex {
        path: path.to_string(),
        defs: collector.defs,
        refs: collector.refs,
        inputs: collector.inputs,
    }
}

struct Collector<'src> {
    source: &'src str,
    lines: &'src Lines,
    depth: usize,
    defs: Vec<Def>,
    refs: Vec<Ref>,
    inputs: Vec<Input>,
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
    fn collect_input(&mut self, expr: &Expr) {
        match expr {
            Expr::Call(call) => {
                let Some(callee) = dotted_name(&call.func) else {
                    return;
                };
                let first = call.arguments.args.first();
                match callee.as_str() {
                    "os.getenv" | "getenv" | "os.environ.get" => {
                        let value = literal_str(first).unwrap_or_else(|| "<dynamic>".into());
                        self.push_input(InputKind::Env, value, expr.range().start());
                    }
                    "open" | "io.open" => {
                        if let Some(path) = literal_str(first) {
                            self.push_input(InputKind::File, path, expr.range().start());
                        }
                    }
                    _ => {}
                }
            }
            Expr::Subscript(sub) if dotted_name(&sub.value).as_deref() == Some("os.environ") => {
                let value = match sub.slice.as_ref() {
                    Expr::StringLiteral(s) => s.value.to_str().to_string(),
                    _ => "<dynamic>".into(),
                };
                self.push_input(InputKind::Env, value, expr.range().start());
            }
            _ => {}
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
                walk_stmt(self, stmt);
                self.depth -= 1;
                return;
            }
            Stmt::ClassDef(c) => {
                self.push_def(c.name.as_str(), DefKind::Class, c.name.start());
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
            }
            Stmt::Import(i) => {
                for alias in &i.names {
                    let bound = alias.asname.as_ref().unwrap_or(&alias.name);
                    self.push_def(bound.as_str(), DefKind::Import, bound.start());
                }
            }
            Stmt::ImportFrom(i) => {
                for alias in &i.names {
                    let bound = alias.asname.as_ref().unwrap_or(&alias.name);
                    self.push_def(bound.as_str(), DefKind::Import, bound.start());
                }
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

/// The dotted path of an attribute/name chain (`os.environ.get`), or `None`
/// for anything more complex (subscripts, calls in the middle, etc.).
fn dotted_name(expr: &Expr) -> Option<String> {
    match expr {
        Expr::Name(n) => Some(n.id.as_str().to_string()),
        Expr::Attribute(a) => Some(format!("{}.{}", dotted_name(&a.value)?, a.attr.as_str())),
        _ => None,
    }
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
