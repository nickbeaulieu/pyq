//! Cross-file resolution behind a trait — the insulation layer.
//!
//! `Resolver` is the contract every query verb depends on. There is **one**
//! query path, not a user-visible engine fork: [`UnifiedResolver`] merges ty's
//! project-wide semantic engine (`ty_ide::find_references` & friends — real
//! binding through imports, re-exports, and aliasing) with the syntactic AST
//! scan from `pyq-index`. ty is authoritative where it sees; the syntactic
//! scan fills the categories ty structurally can't (function-local variables,
//! import bindings), so a `0` from one engine's blind spot can't masquerade as
//! truth. Every [`Loc`] is tagged with its [`Source`] and a `role` so the
//! caller filters one answer instead of choosing between two disagreeing ones.
//!
//! Because ty is early (`0.0.x`), *all* contact with it lives in
//! [`ty_backed`]; if its API moves, only that module changes and the syntactic
//! resolver still answers. `--syntactic` is a debug filter (ty skipped), not a
//! separate semantic contract.

mod syntactic;
mod ty_backed;
mod unified;

pub use syntactic::SyntacticResolver;
pub use ty_backed::TyResolver;
pub use unified::UnifiedResolver;

/// Which engine produced a [`Loc`]. ty results are semantically resolved
/// (precise, cross-file); syntactic results are an over-approximate AST match
/// (a name, not a resolved binding) — useful exactly where ty is blind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    Ty,
    Syntactic,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Source::Ty => "ty",
            Source::Syntactic => "syntactic",
        }
    }
}

/// A resolved source location.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Loc {
    /// Path relative to the project root (best effort).
    pub path: String,
    pub line: usize,
    pub col: usize,
    /// A short tag for the human view (e.g. `"read"`, `"write"`, `"def"`, or —
    /// for `callers` — the enclosing function name).
    pub kind: String,
    /// What this location *is* in the answer, independent of `kind`'s wording:
    /// `"definition"` (canonical), `"binding"` (an `import` that re-binds the
    /// name), `"reference"`, or `"call"`. Lets a caller filter the one merged
    /// result set (`role == "definition"`) instead of picking an engine.
    pub role: &'static str,
    /// Which engine resolved it.
    pub source: Source,
    /// For a `binding`, the canonical definition it resolves to
    /// (`file:line:col`), when unambiguous.
    pub resolves_to: Option<String>,
}

impl Loc {
    /// A `file:line:col` key — the identity used for de-duplication.
    pub fn key(&self) -> String {
        format!("{}:{}:{}", self.path, self.line, self.col)
    }
}

/// The query contract. [`UnifiedResolver`] is the shipping impl; [`TyResolver`]
/// and [`SyntacticResolver`] back it and are usable directly.
pub trait Resolver {
    /// Every reference to `symbol` across the project (cross-file).
    fn references(&self, symbol: &str) -> anyhow::Result<Vec<Loc>>;
    /// Every definition of `symbol`.
    fn definitions(&self, symbol: &str) -> anyhow::Result<Vec<Loc>>;
    /// Every call site of `symbol`; each `Loc`'s `kind` is the enclosing caller.
    fn callers(&self, symbol: &str) -> anyhow::Result<Vec<Loc>>;
}
