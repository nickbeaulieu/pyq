//! Syntactic [`Resolver`]: name matching over per-file [`FileIndex`] facts.
//!
//! No database, no type resolution — it matches a bare identifier against the
//! defs/refs `pyq-index` extracted from each file. That makes it the engine for
//! exactly what ty is blind to: function-local variables (ty resolves only
//! module-and-wider scopes) and `import` bindings (ty reports the canonical
//! origin, not the names that re-bind it). It is *over-approximate* by design —
//! a match is a name, not a resolved binding — so [`Source::Syntactic`] tags
//! every result and [`UnifiedResolver`](crate::UnifiedResolver) only leans on
//! it where ty cannot answer.

use anyhow::Result;
use pyq_index::{DefKind, FileIndex};

use crate::{Loc, Resolver, Source};

pub struct SyntacticResolver {
    files: Vec<FileIndex>,
}

impl SyntacticResolver {
    /// Build over the already-parsed file set (the CLI's tree walk).
    pub fn new(files: Vec<FileIndex>) -> Self {
        SyntacticResolver { files }
    }

    fn refs(&self, symbol: &str, calls_only: bool) -> Vec<Loc> {
        let mut out = Vec::new();
        for f in &self.files {
            for r in &f.refs {
                if r.name != symbol || (calls_only && !r.is_call) {
                    continue;
                }
                out.push(Loc {
                    path: f.path.clone(),
                    line: r.pos.line,
                    col: r.pos.col,
                    kind: if r.is_call { "call" } else { "ref" }.to_string(),
                    role: if r.is_call { "call" } else { "reference" },
                    source: Source::Syntactic,
                    resolves_to: None,
                });
            }
        }
        out
    }
}

impl Resolver for SyntacticResolver {
    fn references(&self, symbol: &str) -> Result<Vec<Loc>> {
        Ok(self.refs(symbol, false))
    }

    fn callers(&self, symbol: &str) -> Result<Vec<Loc>> {
        Ok(self.refs(symbol, true))
    }

    fn definitions(&self, symbol: &str) -> Result<Vec<Loc>> {
        let mut out = Vec::new();
        for f in &self.files {
            for d in &f.defs {
                if d.name != symbol {
                    continue;
                }
                let (kind, role) = match d.kind {
                    DefKind::Function => ("function", "definition"),
                    DefKind::Class => ("class", "definition"),
                    DefKind::Variable => ("variable", "definition"),
                    // An `import` re-binds the name; it is not the origin.
                    DefKind::Import => ("import", "binding"),
                };
                out.push(Loc {
                    path: f.path.clone(),
                    line: d.pos.line,
                    col: d.pos.col,
                    kind: kind.to_string(),
                    role,
                    source: Source::Syntactic,
                    resolves_to: None,
                });
            }
        }
        Ok(out)
    }
}
