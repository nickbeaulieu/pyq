//! ty-backed [`Resolver`]: a `ProjectDatabase` over the root + `ty_ide`.

use anyhow::{anyhow, Context, Result};
use ruff_db::files::{File, FilePath};
use ruff_db::source::source_text;
use ruff_db::system::{OsSystem, SystemPath, SystemPathBuf};
use ruff_text_size::TextSize;
use ty_ide::{find_references, incoming_calls, workspace_symbols};
use ty_project::{ProjectDatabase, ProjectMetadata};

use crate::{Loc, Resolver};

pub struct TyResolver {
    db: ProjectDatabase,
    root: SystemPathBuf,
}

impl TyResolver {
    /// Build a project database rooted at `root` (a path on disk).
    pub fn new(root: &str) -> Result<Self> {
        let abs = std::path::absolute(root).with_context(|| format!("resolving {root}"))?;
        let root = SystemPathBuf::from_path_buf(abs)
            .map_err(|p| anyhow!("non-UTF-8 project path: {}", p.display()))?;
        let system = OsSystem::new(&root);
        let metadata = ProjectMetadata::discover(&root, &system)
            .context("discovering project metadata")?;
        let db = ProjectDatabase::fallible(metadata, system)
            .context("initializing project database")?;
        Ok(TyResolver { db, root })
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

    /// Map a (file, byte offset) to a project-relative `Loc`.
    fn loc(&self, file: File, offset: TextSize, kind: &str) -> Loc {
        let text = source_text(&self.db, file);
        let (line, col) = line_col(text.as_str(), offset.to_usize());
        Loc {
            path: self.rel_path(file),
            line,
            col,
            kind: kind.to_string(),
        }
    }

    fn rel_path(&self, file: File) -> String {
        match file.path(&self.db) {
            FilePath::System(p) => p
                .strip_prefix(&self.root)
                .map(SystemPath::as_str)
                .unwrap_or_else(|_| p.as_str())
                .to_string(),
            other => other.to_string(),
        }
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
                out.push(self.loc(t.file(), t.range().start(), reference_kind(t.kind())));
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
                    out.push(self.loc(call.from.file, range.start(), &caller));
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
            .map(|(file, offset)| self.loc(file, offset, "def"))
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
