//! ty-backed [`Resolver`]: a `ProjectDatabase` over the root + `ty_ide`.

use anyhow::{anyhow, Context, Result};
use ruff_db::files::{File, FilePath};
use ruff_db::source::source_text;
use ruff_db::system::{OsSystem, SystemPathBuf};
use ruff_text_size::TextSize;
use std::collections::HashSet;
use std::path::PathBuf;
use ty_ide::{find_references, incoming_calls, workspace_symbols};
use ty_project::{ProjectDatabase, ProjectMetadata};

use crate::{Loc, Resolver, Source};

pub struct TyResolver {
    db: ProjectDatabase,
    /// Canonical absolute project root; every emitted path is relative to it.
    root_canon: PathBuf,
    /// The files the CLI walk includes. ty resolves against the whole project
    /// for correctness, but only results in this set are *reported* — so the
    /// output honors `--root` and `.gitignore`/hidden filtering, and a nested
    /// worktree copy can't double-count. Empty = report everything (no filter).
    scope: HashSet<PathBuf>,
}

impl TyResolver {
    /// Build a project database rooted at `root` (a path on disk). `scope` is
    /// the set of canonical absolute file paths to report (see [`Self::scope`]);
    /// pass an empty set to disable filtering.
    pub fn new(root: &str, scope: HashSet<PathBuf>) -> Result<Self> {
        let abs = std::path::absolute(root).with_context(|| format!("resolving {root}"))?;
        let root_canon = std::fs::canonicalize(&abs).unwrap_or(abs.clone());
        let root = SystemPathBuf::from_path_buf(abs)
            .map_err(|p| anyhow!("non-UTF-8 project path: {}", p.display()))?;
        let system = OsSystem::new(&root);
        let metadata = ProjectMetadata::discover(&root, &system)
            .context("discovering project metadata")?;
        let db = ProjectDatabase::fallible(metadata, system)
            .context("initializing project database")?;
        Ok(TyResolver {
            db,
            root_canon,
            scope,
        })
    }

    /// Files + offsets of every definition whose name is exactly `symbol`.
    /// `workspace_symbols` is fuzzy, so we filter to exact matches.
    fn exact_symbols(&self, symbol: &str) -> Vec<(File, TextSize)> {
        workspace_symbols(&self.db, symbol)
            .into_iter()
            .filter(|s| s.symbol.name == symbol)
            .map(|s| (s.file, s.symbol.name_range.start()))
            .collect()
    }

    /// Map a (file, byte offset) to a project-relative `Loc`, or `None` if the
    /// file is outside the reporting scope (see [`Self::scope`]). Every ty
    /// location carries [`Source::Ty`].
    fn loc(&self, file: File, offset: TextSize, kind: &str, role: &'static str) -> Option<Loc> {
        let path = self.rel_path(file)?;
        let text = source_text(&self.db, file);
        let (line, col) = line_col(text.as_str(), offset.to_usize());
        Some(Loc {
            path,
            line,
            col,
            kind: kind.to_string(),
            role,
            source: Source::Ty,
            resolves_to: None,
        })
    }

    /// The file's path relative to the canonical root, or `None` if it is not
    /// in scope. A non-system path (typeshed/vendored stdlib stub) is never in
    /// scope. When `scope` is empty, filtering is disabled.
    fn rel_path(&self, file: File) -> Option<String> {
        let FilePath::System(p) = file.path(&self.db) else {
            return None;
        };
        let abs = PathBuf::from(p.as_str());
        let canon = std::fs::canonicalize(&abs).unwrap_or(abs);
        if !self.scope.is_empty() && !self.scope.contains(&canon) {
            return None;
        }
        let rel = canon.strip_prefix(&self.root_canon).unwrap_or(&canon);
        Some(rel.to_string_lossy().into_owned())
    }
}

impl Resolver for TyResolver {
    fn references(&self, symbol: &str) -> Result<Vec<Loc>> {
        let mut out = Vec::new();
        for (file, offset) in self.exact_symbols(symbol) {
            let Some(targets) = find_references(&self.db, file, offset, true) else {
                continue;
            };
            for t in targets {
                if let Some(loc) =
                    self.loc(t.file(), t.range().start(), reference_kind(t.kind()), "reference")
                {
                    out.push(loc);
                }
            }
        }
        dedupe(&mut out);
        Ok(out)
    }

    fn callers(&self, symbol: &str) -> Result<Vec<Loc>> {
        let mut out = Vec::new();
        for (file, offset) in self.exact_symbols(symbol) {
            for call in incoming_calls(&self.db, file, offset) {
                let caller = call.from.name.as_str().to_string();
                for range in call.from_ranges {
                    if let Some(loc) = self.loc(call.from.file, range.start(), &caller, "call") {
                        out.push(loc);
                    }
                }
            }
        }
        dedupe(&mut out);
        Ok(out)
    }

    fn definitions(&self, symbol: &str) -> Result<Vec<Loc>> {
        let mut out: Vec<Loc> = self
            .exact_symbols(symbol)
            .into_iter()
            .filter_map(|(file, offset)| self.loc(file, offset, "def", "definition"))
            .collect();
        dedupe(&mut out);
        Ok(out)
    }
}

fn reference_kind(kind: ty_ide::ReferenceKind) -> &'static str {
    match kind {
        ty_ide::ReferenceKind::Read => "read",
        ty_ide::ReferenceKind::Write => "write",
        _ => "ref",
    }
}

fn dedupe(locs: &mut Vec<Loc>) {
    locs.sort_by(|a, b| {
        (a.path.as_str(), a.line, a.col).cmp(&(b.path.as_str(), b.line, b.col))
    });
    locs.dedup_by(|a, b| a.path == b.path && a.line == b.line && a.col == b.col);
}

/// 1-based line/char-column from a byte offset.
fn line_col(s: &str, byte: usize) -> (usize, usize) {
    let mut line_start = 0usize;
    let mut line = 1usize;
    for (i, b) in s.bytes().enumerate().take(byte) {
        if b == b'\n' {
            line += 1;
            line_start = i + 1;
        }
    }
    let col = s.get(line_start..byte).map_or(1, |seg| seg.chars().count() + 1);
    (line, col)
}
