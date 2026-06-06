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
    /// `true` for a binding nested inside a function/class (not module scope).
    pub nested: bool,
}

/// A use of a name in this file.
#[derive(Clone, Debug, Serialize)]
pub struct Ref {
    pub name: String,
    pub pos: Pos,
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
}
