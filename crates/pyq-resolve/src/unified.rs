//! The one shipping resolver: locate-then-resolve.
//!
//! The syntactic index from `pyq-index` *locates* every place a name is bound
//! or used — including function-locals, parameters, and `import` bindings that a
//! name-level symbol table never surfaces. ty then *resolves* each precise
//! offset semantically. The result is uniformly ty-accurate, scope-aware, and
//! alias-aware, with no over-approximate tier — so a caller sees one answer per
//! verb and never an engine name.
//!
//! Resolution sweeps a name's occurrences (definitions first) and anchors ty at
//! each one *not already covered* by a previously-resolved binding. Because ty
//! resolves a whole binding from any single occurrence, each distinct binding
//! costs exactly one ty call no matter how often it appears — and two
//! same-named bindings (two classes' `process`, two functions' local `x`)
//! resolve separately, each tagged with the def it belongs to.
//!
//! A qualified query (`A.proc`, `pkg.mod.fn`) resolves by its leaf today: it
//! returns every `proc`, each tagged `resolves_to` its def, so a caller filters
//! to the one it means. Scoping the qualifier to a single def needs the index to
//! track each def's container — a tracked follow-up.

use anyhow::Result;
use std::collections::HashSet;
use std::path::PathBuf;

use pyq_index::{DefKind, FileIndex};
use ruff_text_size::TextSize;

use crate::{Loc, Resolver, TyResolver};

pub struct UnifiedResolver {
    ty: TyResolver,
    files: Vec<FileIndex>,
}

/// One place a name appears — a precise offset to hand ty, plus its display key.
struct Anchor {
    path: String,
    offset: u32,
    key: String,
    is_def: bool,
}

/// The bare identifier of a possibly-qualified symbol: `pkg.mod.User` → `User`.
fn leaf(symbol: &str) -> &str {
    symbol.rsplit('.').next().unwrap_or(symbol)
}

impl UnifiedResolver {
    /// `root`/`scope` configure ty (see [`TyResolver::new`]); `files` is the same
    /// tree walk's parsed facts, used to locate anchors.
    pub fn new(root: &str, files: Vec<FileIndex>, scope: HashSet<PathBuf>) -> Result<Self> {
        Ok(UnifiedResolver {
            ty: TyResolver::new(root, scope)?,
            files,
        })
    }

    /// Every occurrence of `name`'s leaf, as ty anchors — canonical definitions
    /// (function/class/variable) first, then every use *and* `import` binding.
    /// Only canonical definitions are `is_def`: an import binding re-binds the
    /// name but isn't a distinct definition, so it must not inflate the "is this
    /// name ambiguous?" count or be mistaken for what a use resolves to.
    fn occurrences(&self, name: &str) -> Vec<Anchor> {
        let name = leaf(name);
        let mut defs = Vec::new();
        let mut uses = Vec::new();
        for f in &self.files {
            for d in &f.defs {
                if d.name == name {
                    let is_def = !matches!(d.kind, DefKind::Import);
                    let anchor = Anchor {
                        path: f.path.clone(),
                        offset: d.offset,
                        key: format!("{}:{}:{}", f.path, d.pos.line, d.pos.col),
                        is_def,
                    };
                    if is_def {
                        defs.push(anchor);
                    } else {
                        uses.push(anchor);
                    }
                }
            }
            for r in &f.refs {
                if r.name == name {
                    uses.push(Anchor {
                        path: f.path.clone(),
                        offset: r.offset,
                        key: format!("{}:{}:{}", f.path, r.pos.line, r.pos.col),
                        is_def: false,
                    });
                }
            }
        }
        defs.extend(uses); // canonical definitions first
        defs
    }

    /// Sweep occurrences, resolving each uncovered binding via `resolve`. When a
    /// name has more than one *canonical* definition, each result is tagged with
    /// the def it resolves to (disambiguating same-named symbols).
    fn sweep(&self, name: &str, resolve: impl Fn(&str, TextSize) -> Vec<Loc>) -> Vec<Loc> {
        let anchors = self.occurrences(name);
        let ambiguous = anchors.iter().filter(|a| a.is_def).count() > 1;
        let mut anchored: HashSet<String> = HashSet::new();
        let mut result_keys: HashSet<String> = HashSet::new();
        let mut out: Vec<Loc> = Vec::new();
        for a in &anchors {
            // Skip if an earlier binding's resolution already covered this spot.
            if anchored.contains(&a.key) || result_keys.contains(&a.key) {
                continue;
            }
            anchored.insert(a.key.clone());
            let owner = (ambiguous && a.is_def).then(|| a.key.clone());
            for mut loc in resolve(&a.path, TextSize::from(a.offset)) {
                if result_keys.insert(loc.key()) {
                    if loc.resolves_to.is_none() {
                        loc.resolves_to = owner.clone();
                    }
                    out.push(loc);
                }
            }
        }
        out.sort_by(|a, b| a.key().cmp(&b.key()));
        out
    }
}

impl Resolver for UnifiedResolver {
    fn references(&self, symbol: &str) -> Result<Vec<Loc>> {
        Ok(self.sweep(symbol, |path, offset| {
            let mut r = self.ty.references_at(path, offset);
            // Every call is a reference; call_hierarchy follows alias renames
            // find_references misses. Relabel as plain `call` references.
            r.extend(self.ty.callers_at(path, offset).into_iter().map(|mut l| {
                l.kind = "call".to_string();
                l
            }));
            r
        }))
    }

    fn callers(&self, symbol: &str) -> Result<Vec<Loc>> {
        Ok(self.sweep(symbol, |path, offset| self.ty.callers_at(path, offset)))
    }

    fn definitions(&self, symbol: &str) -> Result<Vec<Loc>> {
        let name = leaf(symbol);
        let mut defs: Vec<Loc> = Vec::new();
        for f in &self.files {
            for d in &f.defs {
                if d.name != name {
                    continue;
                }
                let (kind, role) = match d.kind {
                    DefKind::Function => ("function", "definition"),
                    DefKind::Class => ("class", "definition"),
                    DefKind::Variable => ("variable", "definition"),
                    // An `import` re-binds the name; it is not the origin.
                    DefKind::Import => ("import", "binding"),
                };
                defs.push(Loc {
                    path: f.path.clone(),
                    line: d.pos.line,
                    col: d.pos.col,
                    kind: kind.to_string(),
                    role,
                    resolves_to: None,
                });
            }
        }
        // Point each binding at the canonical definition when it's unambiguous.
        let canonical: HashSet<String> =
            defs.iter().filter(|l| l.role == "definition").map(Loc::key).collect();
        if canonical.len() == 1 {
            let target = canonical.into_iter().next().unwrap();
            for l in &mut defs {
                if l.role == "binding" {
                    l.resolves_to = Some(target.clone());
                }
            }
        }
        defs.sort_by(|a, b| a.key().cmp(&b.key()));
        defs.dedup_by(|a, b| a.key() == b.key());
        Ok(defs)
    }
}
