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
    Def, DefKind, Effect, EffectKind, FileIndex, ImportContext, ImportStmt, Input, InputKind, Pos,
    Ref,
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
        scope: Vec::new(),
        defs: Vec::new(),
        refs: Vec::new(),
        inputs: Vec::new(),
        imports: Vec::new(),
        effects: Vec::new(),
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
        effects: collector.effects,
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
    /// Enclosing class/function names, outermost first — a def's `container`.
    scope: Vec<String>,
    defs: Vec<Def>,
    refs: Vec<Ref>,
    inputs: Vec<Input>,
    imports: Vec<ImportStmt>,
    effects: Vec<Effect>,
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

    fn push_effect(&mut self, kind: EffectKind, detail: String, offset: TextSize) {
        self.effects.push(Effect {
            kind,
            detail,
            pos: self.pos(offset),
            scope: self.scope.clone(),
            // Anything not inside a function body runs when the module is
            // imported (module scope, or a class body being defined).
            import_time: self.func_depth == 0,
        });
    }

    /// Detect a side effect at a call site, attributed to the enclosing scope.
    /// Matching is on the dotted callee and suffix-based, so it follows the
    /// usual aliases (`import os as o; o.system(...)`, `from subprocess import
    /// run; run(...)`). Over-approximate by design: a syntactic hit means the
    /// code *appears* to perform the effect.
    fn collect_effect(&mut self, expr: &Expr) {
        match expr {
            Expr::Call(call) => {
                if let Some(callee) = dotted_name(&call.func) {
                    if let Some(kind) = classify_effect(&callee) {
                        self.push_effect(kind, callee, expr.range().start());
                    }
                }
            }
            // `os.environ["KEY"]` is an env read even though it isn't a call.
            Expr::Subscript(sub) if is_environ(dotted_name(&sub.value).as_deref()) => {
                let detail = dotted_name(&sub.value).unwrap_or_else(|| "environ".into());
                self.push_effect(EffectKind::Env, detail, expr.range().start());
            }
            _ => {}
        }
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
                    // argparse `parser.add_argument("-v", "--verbose")` and click
                    // `@click.option("-v", "--verbose")` / `@click.argument("name")`.
                    // Multiple alias strings can be passed; record the canonical
                    // long form (`--verbose`) agents search by, not just the first.
                    _ if callee.ends_with(".add_argument")
                        || callee.ends_with(".option")
                        || callee.ends_with(".argument") =>
                    {
                        if let Some(name) = canonical_option(&call.arguments.args) {
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
            offset: offset.to_u32(),
            container: self.scope.clone(),
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
                self.scope.push(f.name.to_string());
                walk_stmt(self, stmt);
                self.scope.pop();
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
                let class_name = c.name.to_string();
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
                self.scope.push(class_name);
                walk_stmt(self, stmt);
                self.scope.pop();
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
            // `global x` inside a function declares intent to rebind a
            // module-level name — a global-state mutation effect. (At module
            // scope it's a no-op, so only count it inside a function.)
            Stmt::Global(g) if self.func_depth > 0 => {
                let names = g.names.iter().map(|n| n.as_str()).collect::<Vec<_>>().join(", ");
                self.push_effect(EffectKind::GlobalState, names, g.start());
            }
            _ => {}
        }
        walk_stmt(self, stmt);
    }

    fn visit_expr(&mut self, expr: &'ast Expr) {
        self.collect_input(expr);
        self.collect_effect(expr);

        // A call's callee, when a bare name, is recorded as a call reference and
        // we skip re-walking it as a plain load (so `f` in `f()` is one ref).
        if let Expr::Call(call) = expr {
            if let Expr::Name(n) = call.func.as_ref() {
                self.refs.push(Ref {
                    name: n.id.as_str().to_string(),
                    pos: self.pos(n.start()),
                    offset: n.start().to_u32(),
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
                    offset: n.start().to_u32(),
                    is_call: false,
                });
            }
        }
        walk_expr(self, expr);
    }
}

/// Classify a dotted callee into the side effect it performs, if any. Suffix-
/// based so it follows aliases (`o.system` for `import os as o`, bare `run` for
/// `from subprocess import run`). Checked in priority order so a callee that
/// could match two buckets lands in the more specific one. Over-approximate:
/// generic method names (`.execute`, `.now`) can hit unrelated code — the
/// `effects` verb flags the surface as static/over-approximate accordingly.
fn classify_effect(callee: &str) -> Option<EffectKind> {
    let ends = |s: &str| callee == s.trim_start_matches('.') || callee.ends_with(s);
    let any = |sigs: &[&str]| sigs.iter().any(|s| ends(s));
    let contains = |s: &str| callee.contains(s);

    // Environment — including the `.get`/`.setdefault` reads on `os.environ`.
    if is_getenv(callee) || ends(".environ.get") || ends(".environ.setdefault") || any(&[".putenv", ".setenv", ".unsetenv"]) {
        return Some(EffectKind::Env);
    }
    // Subprocess / shell.
    if contains("subprocess.") || any(&[".system", ".popen", ".execv", ".execve", ".execvp", ".spawnl", ".spawnv"]) {
        return Some(EffectKind::Subprocess);
    }
    // Network.
    if contains("requests.")
        || contains("httpx.")
        || contains("aiohttp.")
        || contains("urllib.request")
        || ends(".urlopen")
        || contains("socket.")
        || ends(".socket")
        || contains("http.client")
    {
        return Some(EffectKind::Network);
    }
    // Filesystem. `os.*` mutators are matched as written (the common spelling);
    // `read_text`/`write_text`/`…_bytes` catch `Path`/handle methods.
    if callee == "open"
        || ends("io.open")
        || any(&[".read_text", ".write_text", ".read_bytes", ".write_bytes"])
        || any(&[
            "os.remove", "os.unlink", "os.mkdir", "os.makedirs", "os.rmdir",
            "os.rename", "os.replace",
        ])
        || contains("shutil.")
    {
        return Some(EffectKind::Fs);
    }
    // Database (over-approximate — generic execute/cursor names).
    if any(&[".execute", ".executemany", ".executescript", ".fetchone", ".fetchall"])
        || ends("sqlite3.connect")
        || ends("psycopg2.connect")
        || ends("pymysql.connect")
        || contains("sqlalchemy.")
    {
        return Some(EffectKind::Db);
    }
    // Randomness / non-determinism.
    if contains("random.") || contains("secrets.") || ends("os.urandom") || contains("uuid.") || any(&[".uuid4", ".uuid1"]) {
        return Some(EffectKind::Random);
    }
    // Wall clock.
    if any(&[
        "time.time", "time.monotonic", "time.perf_counter", "time.process_time",
        "time.sleep", "time.gmtime", "time.localtime",
    ]) || any(&[".now", ".utcnow", ".today", ".fromtimestamp"])
    {
        return Some(EffectKind::Clock);
    }
    None
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

/// The canonical name of a CLI option/argument from its positional strings:
/// the longest `--long` flag if any (what callers search by), else the first
/// string literal (a short flag or a positional name like `path`).
fn canonical_option(args: &[Expr]) -> Option<String> {
    let names: Vec<String> = args.iter().filter_map(|a| literal_str(Some(a))).collect();
    names
        .iter()
        .filter(|n| n.starts_with("--"))
        .max_by_key(|n| n.len())
        .or_else(|| names.first())
        .cloned()
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
