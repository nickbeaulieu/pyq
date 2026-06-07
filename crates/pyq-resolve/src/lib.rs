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

mod graph;
mod ty_backed;
mod unified;

pub use graph::{scope_fqn, CallGraph, Closure, Direction, GraphNode};

/// One immediate base class of a class, resolved by ty. `anchor` is the base's
/// `(path, name offset)` when it's a first-party class in scope; `None` when the
/// base is external (third-party/stdlib) or couldn't be resolved — the signal a
/// class is framework-managed (it extends something pyq can't see into).
#[derive(Clone, Debug)]
pub struct SuperClass {
    pub name: String,
    pub anchor: Option<(String, u32)>,
}

/// Whether a name is a top-level member of a resolved module — the answer to
/// "does `patch("pkg.mod.time.sleep")` point at something real," for the case
/// where the tail attribute sits on an imported *module* (ty resolves the import
/// into typeshed/site-packages, and we read that module's surface).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemberCheck {
    /// The module declares the member at top level.
    Present,
    /// The module was resolved and read, and the member is absent — and the
    /// module has no `__getattr__`, so it can't appear dynamically either.
    Absent,
    /// Couldn't resolve the binding to a readable module (not a module binding,
    /// unresolved, or a module with a dynamic `__getattr__`).
    Unknown,
}
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

/// A callable that ty resolved as a call-graph neighbor (a callee, or a caller).
///
/// Unlike [`Loc`], it carries the callable's *name byte offset* — the precise,
/// durable anchor the graph traversal re-feeds to ty to recurse, and the same
/// offset the syntactic index records for the def, so a neighbor maps straight
/// back to its stable fully-qualified id.
#[derive(Clone, Debug)]
pub struct Neighbor {
    /// Path relative to the project root.
    pub path: String,
    /// Byte offset of the callable's name (its `selection_range` start).
    pub offset: u32,
    pub line: usize,
    pub col: usize,
    /// The callable's short name (`make_user`, `__init__`).
    pub name: String,
    /// ty's symbol kind, lowercased (`"function"`, `"method"`, `"class"`, …).
    pub kind: &'static str,
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
