//! The per-file fact model produced by extraction.
//!
//! Single-file and name-based for now: definitions and references are matched
//! by identifier within one module. Cross-file resolution (import edges,
//! qualified names) is the next layer and will live alongside this without
//! changing these types.

use serde::Serialize;

/// 1-based line/column. Columns are UTF-8 character columns, not byte offsets.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct Pos {
    pub line: usize,
    pub col: usize,
}

/// What kind of binding a definition is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DefKind {
    Function,
    Class,
    Variable,
    Import,
}

/// A name binding introduced in this file.
#[derive(Clone, Debug, Serialize)]
pub struct Def {
    pub name: String,
    pub kind: DefKind,
    pub pos: Pos,
    /// Byte offset of the bound name in the source — the precise anchor a
    /// semantic engine resolves from (so even a function-local, invisible to
    /// name-level symbol tables, can be resolved exactly).
    pub offset: u32,
    /// Dotted names of the enclosing scopes (classes/functions), outermost
    /// first — `Alpha` for `Alpha.process`, empty at module scope. Lets a
    /// qualified query (`Alpha.process`) scope to the right def.
    pub container: Vec<String>,
    /// `true` for a binding nested inside a function/class (not module scope).
    pub nested: bool,
    /// For a [`Class`](DefKind::Class), the dotted names of its base classes
    /// (`["AbstractUser"]`, `["models.Model"]`); empty for non-classes and for a
    /// class with no bases. Lets a consumer know a class may inherit members it
    /// can't see (so a missing attribute isn't necessarily absent).
    pub bases: Vec<String>,
}

/// A use of a name in this file.
#[derive(Clone, Debug, Serialize)]
pub struct Ref {
    pub name: String,
    pub pos: Pos,
    /// Byte offset of the name — a precise anchor for resolving the binding this
    /// use refers to, even when the binding (a param, loop/with/except target)
    /// is never recorded as a [`Def`].
    pub offset: u32,
    /// `true` when this name is the callee of a call expression (`name(...)`).
    pub is_call: bool,
}

/// An external input the module depends on — part of "what does this need to
/// run." Syntactic and over-approximate by design (computed keys/paths are
/// bucketed or omitted, never guessed).
#[derive(Clone, Debug, Serialize)]
pub struct Input {
    pub kind: InputKind,
    /// The literal name/path, or `<dynamic>` when the key/path is computed.
    pub value: String,
    pub pos: Pos,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum InputKind {
    /// An environment variable read (`os.getenv`, `os.environ[...]`, `.get`).
    Env,
    /// A literal filesystem path opened (`open("...")`).
    File,
    /// A CLI argument (argparse `add_argument`, click `@option`/`@argument`).
    Arg,
    /// A settings field on a pydantic `BaseSettings` subclass.
    Setting,
}

/// A category of observable side effect a piece of code performs. Syntactic and
/// over-approximate by design (matched on call-site shape, alias-following), so
/// a hit means "statically appears to" — the basis for "is this pure / safe in
/// a test / will it hit the network."
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum EffectKind {
    /// Filesystem: `open`, `Path.read_text`/`write_text`, `os.remove`, `shutil.*`.
    Fs,
    /// Network: `requests`/`httpx`/`urllib`/`socket`/`aiohttp`.
    Network,
    /// Subprocess / shell: `subprocess.*`, `os.system`, `os.popen`.
    Subprocess,
    /// Environment: `os.getenv`, `os.environ[...]`, `os.putenv`.
    Env,
    /// Database: `*.execute`/`executemany`, `sqlite3`/`psycopg2`/`pymysql.connect`.
    Db,
    /// Non-determinism: `random.*`, `secrets.*`, `os.urandom`, `uuid.uuid4`.
    Random,
    /// Wall clock: `time.time`/`sleep`/`monotonic`, `datetime.now`/`utcnow`.
    Clock,
    /// Module-global mutation: a `global` declaration inside a function.
    #[serde(rename = "global")]
    GlobalState,
}

/// One side effect a piece of code statically appears to perform.
#[derive(Clone, Debug, Serialize)]
pub struct Effect {
    pub kind: EffectKind,
    /// The matched API or signal (`requests.get`, `os.environ`, `global x`).
    pub detail: String,
    pub pos: Pos,
    /// Enclosing class/function names (outermost first) — the owner whose body
    /// performs the effect. Combined with the module path this is the owner's
    /// fully-qualified id, so an effect maps onto a call-graph node.
    pub scope: Vec<String>,
    /// `true` when the effect runs at *import time* — it sits at module or
    /// class-body scope (not inside a function), so merely importing the module
    /// triggers it.
    pub import_time: bool,
}

/// When an import executes — distinguishes the module-load-time edges that form
/// real import cycles from the ones good code uses to *break* them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ImportContext {
    /// Module scope, not under a `TYPE_CHECKING` guard — runs at import time.
    TopLevel,
    /// Under `if TYPE_CHECKING:` — never executes at runtime (type-only).
    TypeChecking,
    /// Inside a function body — lazy, runs only when that function is called.
    Deferred,
}

/// An import statement's module target — an edge in the dependency graph.
/// Captures the written module and relative-import depth; name binding is
/// already recorded separately as an [`Import`](DefKind::Import) def.
#[derive(Clone, Debug, Serialize)]
pub struct ImportStmt {
    /// The dotted module as written (`pkg.models`). Empty for `from . import x`,
    /// where the targets are the imported `names` under the resolved package.
    pub module: String,
    /// Leading-dot count for a relative import (`0` = absolute).
    pub level: u32,
    /// The imported names, used only to resolve `from <pkg> import <name>` into
    /// submodule edges; empty for plain `import x`.
    pub names: Vec<String>,
    /// When this import runs — only [`TopLevel`](ImportContext::TopLevel) edges
    /// count toward import cycles.
    pub context: ImportContext,
    pub pos: Pos,
}

/// A `mock.patch("target")` call site — the dotted lookup path the patch
/// replaces at runtime. Resolving it against the project flags drifted/typo'd
/// paths: a patch whose target no longer exists silently does nothing, and the
/// test passes while testing the real code. `target` is `None` when the first
/// argument isn't a string literal (computed → not statically verifiable).
#[derive(Clone, Debug, Serialize)]
pub struct MockTarget {
    pub target: Option<String>,
    pub pos: Pos,
}

/// All facts extracted from one Python module.
#[derive(Clone, Debug, Serialize)]
pub struct FileIndex {
    pub path: String,
    pub defs: Vec<Def>,
    pub refs: Vec<Ref>,
    pub inputs: Vec<Input>,
    pub imports: Vec<ImportStmt>,
    pub effects: Vec<Effect>,
    pub mocks: Vec<MockTarget>,
}
