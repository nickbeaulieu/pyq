//! The one query path: ty merged with the syntactic scan.
//!
//! ty is authoritative wherever it resolves — precise, cross-file, alias-aware.
//! But each engine has a blind spot the other covers, and *neither is a
//! superset of the truth*: ty cannot see function-local variables (returns
//! nothing), the syntactic scan cannot see attribute-access calls
//! (`obj.method()`). Run alone, either engine's blind spot produces a silent
//! `0` that reads as "unused / safe to delete." So we run both and merge:
//!
//! - **refs / callers** — union, de-duplicated by location, ty kept on overlap.
//!   ty's precise hits stand; the syntactic scan only *adds* locations ty never
//!   reported (e.g. uses of a function-local variable), each tagged
//!   [`Source::Syntactic`] so the caller knows it is over-approximate.
//! - **defs** — one answer with a `role`: canonical `definition`s (ty's origin,
//!   plus any local the syntactic scan sees that ty missed) and the `import`
//!   `binding`s that re-bind the name, each pointing at the canonical def via
//!   `resolves_to`. The old 1-vs-36 engine split becomes a single set the
//!   caller filters (`role == "definition"`).

use anyhow::Result;
use std::collections::HashSet;
use std::path::PathBuf;

use pyq_index::FileIndex;

use crate::{Loc, Resolver, SyntacticResolver, TyResolver};

pub struct UnifiedResolver {
    ty: TyResolver,
    syntactic: SyntacticResolver,
}

impl UnifiedResolver {
    /// `root`/`scope` configure the ty engine (see [`TyResolver::new`]); `files`
    /// is the same tree walk's parsed facts for the syntactic engine.
    pub fn new(root: &str, files: Vec<FileIndex>, scope: HashSet<PathBuf>) -> Result<Self> {
        Ok(UnifiedResolver {
            ty: TyResolver::new(root, scope)?,
            syntactic: SyntacticResolver::new(files),
        })
    }

    /// ty results, plus syntactic results at locations ty never reported.
    /// ty wins on overlap (its `kind`/role carry the resolved meaning).
    fn union_prefer_ty(ty: Vec<Loc>, syn: Vec<Loc>) -> Vec<Loc> {
        let seen: HashSet<String> = ty.iter().map(Loc::key).collect();
        let mut out = ty;
        for loc in syn {
            if !seen.contains(&loc.key()) {
                out.push(loc);
            }
        }
        sort_dedupe(&mut out);
        out
    }
}

impl Resolver for UnifiedResolver {
    fn references(&self, symbol: &str) -> Result<Vec<Loc>> {
        let mut ty = self.ty.references(symbol)?;
        // Every call is a reference. ty's `call_hierarchy` follows `import as` /
        // re-export renames that `find_references` misses, so fold the call
        // sites in — otherwise `refs` under-reports an aliased symbol that
        // `callers` finds, and `callers ⊄ refs` despite the docs. Relabel as a
        // plain `call` reference (drop the enclosing-function name `callers`
        // uses); de-dup by location drops any already found as a reference.
        ty.extend(self.ty.callers(symbol)?.into_iter().map(|mut l| {
            l.kind = "call".to_string();
            l
        }));
        Ok(Self::union_prefer_ty(ty, self.syntactic.references(symbol)?))
    }

    fn callers(&self, symbol: &str) -> Result<Vec<Loc>> {
        Ok(Self::union_prefer_ty(
            self.ty.callers(symbol)?,
            self.syntactic.callers(symbol)?,
        ))
    }

    fn definitions(&self, symbol: &str) -> Result<Vec<Loc>> {
        let ty_defs = self.ty.definitions(symbol)?;
        let syn = self.syntactic.definitions(symbol)?;
        let (syn_defs, mut bindings): (Vec<Loc>, Vec<Loc>) =
            syn.into_iter().partition(|l| l.role == "definition");

        // Canonical definitions: ty's origin, plus any definition the syntactic
        // scan sees that ty missed (e.g. a function-local variable).
        let mut defs = Self::union_prefer_ty(ty_defs, syn_defs);

        // When the canonical origin is unambiguous, point every binding at it.
        if defs.len() == 1 {
            let target = defs[0].key();
            for b in &mut bindings {
                b.resolves_to = Some(target.clone());
            }
        }

        defs.append(&mut bindings);
        sort_dedupe(&mut defs);
        Ok(defs)
    }
}

fn sort_dedupe(locs: &mut Vec<Loc>) {
    locs.sort_by(|a, b| (a.path.as_str(), a.line, a.col).cmp(&(b.path.as_str(), b.line, b.col)));
    locs.dedup_by(|a, b| a.path == b.path && a.line == b.line && a.col == b.col);
}
