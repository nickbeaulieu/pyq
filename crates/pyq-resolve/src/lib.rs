//! Cross-file resolution behind a trait — the insulation layer.
//!
//! `Resolver` is the contract every query verb depends on. The shipping impl is
//! [`TyResolver`], which drives ty's project-wide semantic engine
//! (`ty_ide::find_references` & friends) — real binding through imports,
//! re-exports, and aliasing. Because ty is early (`0.0.x`), *all* contact with
//! it lives here; if its API moves, only this crate changes, and the syntactic
//! extractor in `pyq-index` can implement the same trait as a fallback.

mod ty_backed;

pub use ty_backed::TyResolver;

/// A resolved source location.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Loc {
    /// Path relative to the project root (best effort).
    pub path: String,
    pub line: usize,
    pub col: usize,
    /// A short tag for the human view (e.g. `"read"`, `"write"`, `"def"`).
    pub kind: String,
}

/// The query contract. ty backs it today; the syntactic extractor can later.
pub trait Resolver {
    /// Every reference to `symbol` across the project (cross-file).
    fn references(&self, symbol: &str) -> anyhow::Result<Vec<Loc>>;
    /// Every definition of `symbol`.
    fn definitions(&self, symbol: &str) -> anyhow::Result<Vec<Loc>>;
    /// Every call site of `symbol`; each `Loc`'s `kind` is the enclosing caller.
    fn callers(&self, symbol: &str) -> anyhow::Result<Vec<Loc>>;
}
