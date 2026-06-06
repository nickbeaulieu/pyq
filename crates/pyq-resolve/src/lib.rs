//! Cross-file resolution behind a trait — the insulation layer.
//!
//! `Resolver` is the contract every query verb depends on, and there is exactly
//! one shipping impl: [`UnifiedResolver`]. A caller never picks an engine or
//! sees one named. Under the hood it's *locate-then-resolve*: the syntactic
//! index from `pyq-index` locates every place a name is bound or used (it knows
//! offsets for function-locals, params, and import bindings that a name-level
//! symbol table never surfaces), and ty resolves each of those precise offsets
//! semantically — real binding through imports, re-exports, and aliasing, scope
//! and all. So every result is ty-resolved and exact; there is no
//! over-approximate tier to disclose.
//!
//! Because ty is early (`0.0.x`), *all* contact with it lives in [`ty_backed`];
//! if its API moves, only that module changes.

mod ty_backed;
mod unified;

pub use ty_backed::TyResolver;
pub use unified::UnifiedResolver;

/// A resolved source location.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Loc {
    /// Path relative to the project root.
    pub path: String,
    pub line: usize,
    pub col: usize,
    /// A short tag for the human view (e.g. `"read"`, `"write"`, `"def"`, or —
    /// for `callers` — the enclosing function name).
    pub kind: String,
    /// What this location *is*, independent of `kind`'s wording: `"definition"`
    /// (canonical), `"binding"` (an `import` that re-binds the name),
    /// `"reference"`, or `"call"`. Lets a caller filter one result set
    /// (`role == "definition"`).
    pub role: &'static str,
    /// For a `binding` — or any use of an ambiguous (same-named) symbol — the
    /// canonical definition it resolves to (`file:line:col`), when unambiguous.
    pub resolves_to: Option<String>,
}

impl Loc {
    /// A `file:line:col` key — the identity used for de-duplication.
    pub fn key(&self) -> String {
        format!("{}:{}:{}", self.path, self.line, self.col)
    }
}

/// The query contract. [`UnifiedResolver`] is the shipping impl; [`TyResolver`]
/// backs it and is usable directly.
pub trait Resolver {
    /// Every reference to `symbol` across the project (cross-file).
    fn references(&self, symbol: &str) -> anyhow::Result<Vec<Loc>>;
    /// Every definition of `symbol`.
    fn definitions(&self, symbol: &str) -> anyhow::Result<Vec<Loc>>;
    /// Every call site of `symbol`; each `Loc`'s `kind` is the enclosing caller.
    fn callers(&self, symbol: &str) -> anyhow::Result<Vec<Loc>>;
}
