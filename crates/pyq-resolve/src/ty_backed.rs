//! ty-backed [`Resolver`]: a `ProjectDatabase` over the root + `ty_ide`.

use anyhow::{anyhow, Context, Result};
use ruff_db::files::{system_path_to_file, File, FilePath};
use ruff_db::source::source_text;
use ruff_db::system::{OsSystem, SystemPath, SystemPathBuf};
use ruff_text_size::{TextRange, TextSize};
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use ty_ide::{
    find_references, goto_definition, incoming_calls, outgoing_calls, type_hierarchy_supertypes,
    workspace_symbols, CallHierarchyItem, SymbolKind,
};
use ty_project::metadata::options::{EnvironmentOptions, Options, ProjectOptionsOverrides};
use ty_project::metadata::value::RelativePathBuf;
use ty_project::{ProjectDatabase, ProjectMetadata};

use crate::{Loc, Neighbor, Resolver};

/// `Clone` is cheap — `ProjectDatabase` is an `Arc`-backed salsa handle and
/// `scope` is shared behind an `Arc` — so the recording can hand each rayon
/// task its own clone (ty's databases aren't `Sync`, but a per-thread clone
/// gives the same shared storage with independent query state).
#[derive(Clone)]
pub struct TyResolver {
    db: ProjectDatabase,
    /// Canonical absolute project root; every emitted path is relative to it.
    root_canon: PathBuf,
    /// The files the CLI walk includes. ty resolves against the whole project
    /// for correctness, but only results in this set are *reported* — so the
    /// output honors `--root` and `.gitignore`/hidden filtering, and a nested
    /// worktree copy can't double-count. Empty = report everything (no filter).
    /// `Arc` so a per-task clone is a refcount bump, not a set copy.
    scope: Arc<HashSet<PathBuf>>,
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
        let mut metadata = ProjectMetadata::discover(&root, &system)
            .context("discovering project metadata")?;

        // Honor source roots the runtime uses but ty doesn't discover on its own.
        // ty auto-detects `./src` and `./<project>` layouts and reads `PYTHONPATH`,
        // but a very common Python convention puts the import root behind pytest's
        // `[tool.pytest.ini_options] pythonpath` (e.g. `pythonpath = ["mroi_matcher"]`,
        // so first-party code imports `helpers.validators`, not
        // `mroi_matcher.helpers.validators`). Without this, those imports don't
        // resolve and `refs`/`callers` silently under-report. We feed the declared
        // paths to ty as `extra-paths` — additive, so ty keeps its own auto-detected
        // roots (incl. the project root) and just gains these.
        let src_roots = pytest_pythonpath(metadata.root().as_str());
        if !src_roots.is_empty() {
            let overrides = ProjectOptionsOverrides {
                options: Options {
                    environment: Some(EnvironmentOptions {
                        extra_paths: Some(
                            src_roots.iter().map(RelativePathBuf::cli).collect(),
                        ),
                        ..EnvironmentOptions::default()
                    }),
                    ..Options::default()
                },
                ..ProjectOptionsOverrides::default()
            };
            metadata.apply_overrides(&overrides);
        }

        let db = ProjectDatabase::fallible(metadata, system)
            .context("initializing project database")?;
        Ok(TyResolver {
            db,
            root_canon,
            scope: Arc::new(scope),
        })
    }

    /// Files + offsets of every definition matching `symbol`. A bare name
    /// (`proc`) matches every def of that name; a *qualified* name (`A.proc`,
    /// `Outer.Inner.m`) honors the qualifier — only defs whose enclosing scopes
    /// match the prefix, by source-range containment, are kept. So `A.proc` and
    /// `B.proc` no longer collapse to the same answer.
    fn exact_symbols(&self, symbol: &str) -> Vec<(File, TextSize)> {
        let mut parts: Vec<&str> = symbol.split('.').collect();
        let leaf = parts.pop().unwrap_or(symbol); // never empty: split yields ≥1
        let quals = parts; // enclosing scopes, outermost first
        self.symbols_named(leaf)
            .into_iter()
            .filter(|(file, _, full)| quals.is_empty() || self.enclosed_by(*file, *full, &quals))
            .map(|(file, start, _)| (file, start))
            .collect()
    }

    /// Every symbol named exactly `name` (file, name offset, full body range).
    /// `workspace_symbols` is fuzzy, so we filter to exact matches.
    fn symbols_named(&self, name: &str) -> Vec<(File, TextSize, TextRange)> {
        workspace_symbols(&self.db, name)
            .into_iter()
            .filter(|s| s.symbol.name == name)
            .map(|s| (s.file, s.symbol.name_range.start(), s.symbol.full_range))
            .collect()
    }

    /// Whether the def occupying `target` in `file` is enclosed by a chain of
    /// scopes named `quals` (outermost first) — e.g. `["A"]` requires a symbol
    /// `A` whose body contains `target`. Climbs inner→outer, at each step picking
    /// the tightest containing symbol of the expected name; any gap fails.
    fn enclosed_by(&self, file: File, target: TextRange, quals: &[&str]) -> bool {
        let mut inner = target;
        for name in quals.iter().rev() {
            let container = self
                .symbols_named(name)
                .into_iter()
                .filter(|(f, _, full)| *f == file && *full != inner && full.contains_range(inner))
                .min_by_key(|(_, _, full)| full.len());
            match container {
                Some((_, _, full)) => inner = full,
                None => return false,
            }
        }
        true
    }

    /// The ty `File` for a path relative to the project root, if ty indexed it.
    fn file_at(&self, rel_path: &str) -> Option<File> {
        let abs = self.root_canon.join(rel_path);
        let sys = SystemPath::new(abs.to_str()?).to_path_buf();
        system_path_to_file(&self.db, &sys).ok()
    }

    /// References to whatever binding sits at `offset` in `rel_path` — anchored
    /// on a precise location, not a name. This is the linchpin of the
    /// locate-then-resolve design: handed a definition (or use) offset — which
    /// the syntactic index knows for *every* binding, including function-locals
    /// and params that `workspace_symbols` never surfaces — ty resolves that
    /// exact binding, scope and all.
    pub fn references_at(&self, rel_path: &str, offset: TextSize) -> Vec<Loc> {
        let Some(file) = self.file_at(rel_path) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        if let Some(targets) = find_references(&self.db, file, offset, true) {
            for t in targets {
                if let Some(loc) =
                    self.loc(t.file(), t.range().start(), reference_kind(t.kind()), "reference")
                {
                    out.push(loc);
                }
            }
        }
        dedupe(&mut out);
        out
    }

    /// Call sites of whatever binding sits at `offset` in `rel_path`, each
    /// labelled with its enclosing function. ty's `call_hierarchy` follows
    /// `import as`/re-export renames, so anchoring here catches alias call sites
    /// `find_references` misses.
    pub fn callers_at(&self, rel_path: &str, offset: TextSize) -> Vec<Loc> {
        let Some(file) = self.file_at(rel_path) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for call in incoming_calls(&self.db, file, offset) {
            let caller = call.from.name.as_str().to_string();
            for range in call.from_ranges {
                if let Some(loc) = self.loc(call.from.file, range.start(), &caller, "call") {
                    out.push(loc);
                }
            }
        }
        dedupe(&mut out);
        out
    }

    /// The callables directly *called by* whatever sits at `offset` in
    /// `rel_path` — its outgoing call-graph edges. Each [`Neighbor`] carries the
    /// callee's own name offset, so the graph traversal recurses into it without
    /// re-grepping. Out-of-scope callees (stdlib, third-party) are dropped, so
    /// the graph stays project-internal. ty folds repeat call sites to one
    /// callee into a single entry.
    pub fn outgoing_at(&self, rel_path: &str, offset: TextSize) -> Vec<Neighbor> {
        let Some(file) = self.file_at(rel_path) else {
            return Vec::new();
        };
        outgoing_calls(&self.db, file, offset)
            .into_iter()
            .filter_map(|c| self.neighbor(&c.to))
            .collect()
    }

    /// The callables that directly *call* whatever sits at `offset` in
    /// `rel_path` — its incoming call-graph edges (one [`Neighbor`] per caller,
    /// not per call site). Counterpart of [`Self::outgoing_at`] for the reverse
    /// closure; out-of-scope callers are dropped.
    ///
    /// Anchored at a single offset. ty's `incoming_calls` from a *definition*
    /// misses callers that reach the symbol through an import (`from m import f`),
    /// so the graph layer anchors this at every occurrence that resolves to the
    /// node — see `CallGraph`.
    pub fn incoming_at(&self, rel_path: &str, offset: TextSize) -> Vec<Neighbor> {
        let Some(file) = self.file_at(rel_path) else {
            return Vec::new();
        };
        let mut seen: HashSet<(String, u32)> = HashSet::new();
        let mut out = Vec::new();
        for call in incoming_calls(&self.db, file, offset) {
            if let Some(nb) = self.neighbor(&call.from) {
                if seen.insert((nb.path.clone(), nb.offset)) {
                    out.push(nb);
                }
            }
        }
        out
    }

    /// The immediate base classes of the class at `(rel_path, offset)`, resolved
    /// by ty's type hierarchy. Each carries its name and — when it's a
    /// first-party class in scope — its `(path, name offset)` anchor; an external
    /// or unresolved base has `anchor: None`. `object` is not reported (ty omits
    /// the implicit base), so an empty result means "no explicit base."
    pub fn supertypes_at(&self, rel_path: &str, offset: u32) -> Vec<crate::SuperClass> {
        let Some(file) = self.file_at(rel_path) else {
            return Vec::new();
        };
        type_hierarchy_supertypes(&self.db, file, TextSize::from(offset))
            .into_iter()
            .map(|item| {
                let anchor = self
                    .rel_path(item.file)
                    .map(|p| (p, item.selection_range.start().to_u32()));
                crate::SuperClass {
                    name: item.name.as_str().to_string(),
                    anchor,
                }
            })
            .collect()
    }

    /// Resolve the binding referenced at `offset` in `rel_path` to its
    /// definition's `(path, name offset)` — the durable anchor a use site points
    /// at. Used to attribute a syntactic occurrence to a specific graph node
    /// (so two same-named symbols don't merge). `None` if it resolves out of
    /// scope or doesn't resolve. Follows import aliases.
    pub fn resolve_def_at(&self, rel_path: &str, offset: TextSize) -> Option<(String, u32)> {
        let file = self.file_at(rel_path)?;
        let targets = goto_definition(&self.db, file, offset)?;
        let target = targets.into_iter().next()?;
        let path = self.rel_path(target.file())?;
        Some((path, target.focus_range().start().to_u32()))
    }

    /// Whether `member` is a top-level name of the *module* that the binding at
    /// `(rel_path, offset)` resolves to. This is how `mock-targets` verifies a
    /// patch whose tail attribute is on an imported module (`patch("m.time.sleep")`
    /// → resolve `time`, look up `sleep`): ty navigates the import into typeshed
    /// (or site-packages), and we read that module's surface — reaching into
    /// third-party code the project-local index can't see.
    ///
    /// Gated to genuine module bindings: ty reports a module navigation target as
    /// an empty range at file start (`0..0`), versus a real symbol whose range
    /// covers its def — so a `from m import func` binding (func is a value, not a
    /// module) returns [`MemberCheck::Unknown`] and stays unverifiable rather than
    /// being checked against the wrong namespace.
    pub fn module_member(&self, rel_path: &str, offset: u32, member: &str) -> crate::MemberCheck {
        use crate::MemberCheck;
        let Some(file) = self.file_at(rel_path) else {
            return MemberCheck::Unknown;
        };
        let Some(target) =
            goto_definition(&self.db, file, TextSize::from(offset)).and_then(|t| t.into_iter().next())
        else {
            return MemberCheck::Unknown;
        };
        // A module target is an empty range at the file start; a real symbol has
        // a non-empty range over its definition.
        let fr = target.full_range();
        if !(fr.start() == fr.end() && fr.start().to_u32() == 0) {
            return MemberCheck::Unknown;
        }
        let module_file = target.file();
        let path = match module_file.path(&self.db) {
            FilePath::System(p) => p.as_str().to_string(),
            FilePath::Vendored(p) => p.as_str().to_string(),
            FilePath::SystemVirtual(_) => return MemberCheck::Unknown,
        };
        if !(path.ends_with(".pyi") || path.ends_with(".py")) {
            return MemberCheck::Unknown;
        }
        // Read the module's surface and check for the member. A module-level
        // `__getattr__` (PEP 562) means attributes can appear dynamically, so we
        // can't prove absence — stay Unknown in that case.
        let text = source_text(&self.db, module_file);
        let idx = pyq_index::extract(&path, text.as_str());
        let top = |name: &str| {
            idx.defs
                .iter()
                .any(|d| d.container.is_empty() && d.name == name)
        };
        if top(member) {
            MemberCheck::Present
        } else if top("__getattr__") {
            MemberCheck::Unknown
        } else {
            MemberCheck::Absent
        }
    }

    /// Turn a call-hierarchy item into a [`Neighbor`], or `None` if it is out of
    /// the reporting scope (typeshed, third-party, a `--root`-excluded file).
    /// Anchors on the item's `selection_range` — the symbol *name* range, the
    /// same offset the syntactic index records for the def, so the two line up.
    fn neighbor(&self, item: &CallHierarchyItem) -> Option<Neighbor> {
        let path = self.rel_path(item.file)?;
        let offset = item.selection_range.start();
        let text = source_text(&self.db, item.file);
        let (line, col) = line_col(text.as_str(), offset.to_usize());
        Some(Neighbor {
            path,
            offset: offset.to_u32(),
            line,
            col,
            name: item.name.as_str().to_string(),
            kind: symbol_kind(item.kind),
        })
    }

    /// Map a (file, byte offset) to a project-relative `Loc`, or `None` if the
    /// file is outside the reporting scope (see [`Self::scope`]).
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
            resolves_to: None,
        })
    }

    /// The `file:line:col` of a def, used to tag the uses that resolve to it —
    /// but only when the name is `ambiguous` (multiple defs), so single-def
    /// queries stay uncluttered.
    fn owner_key(&self, ambiguous: bool, file: File, offset: TextSize) -> Option<String> {
        if !ambiguous {
            return None;
        }
        self.loc(file, offset, "def", "definition").map(|l| l.key())
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
        let defs = self.exact_symbols(symbol);
        // When a bare name has several defs (two classes' `process`), ty
        // resolves each use to a *specific* one — so attribute every result to
        // the def it resolves to, instead of unioning them indistinguishably.
        let ambiguous = defs.len() > 1;
        let mut out = Vec::new();
        for (file, offset) in defs {
            let owner = self.owner_key(ambiguous, file, offset);
            let Some(targets) = find_references(&self.db, file, offset, true) else {
                continue;
            };
            for t in targets {
                if let Some(mut loc) =
                    self.loc(t.file(), t.range().start(), reference_kind(t.kind()), "reference")
                {
                    loc.resolves_to = owner.clone();
                    out.push(loc);
                }
            }
        }
        dedupe(&mut out);
        Ok(out)
    }

    fn callers(&self, symbol: &str) -> Result<Vec<Loc>> {
        let defs = self.exact_symbols(symbol);
        let ambiguous = defs.len() > 1;
        let mut out = Vec::new();
        for (file, offset) in defs {
            let owner = self.owner_key(ambiguous, file, offset);
            for call in incoming_calls(&self.db, file, offset) {
                let caller = call.from.name.as_str().to_string();
                for range in call.from_ranges {
                    if let Some(mut loc) = self.loc(call.from.file, range.start(), &caller, "call") {
                        loc.resolves_to = owner.clone();
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

/// Source roots declared via pytest's `[tool.pytest.ini_options] pythonpath` in
/// `<project_root>/pyproject.toml`. `pythonpath` may be a string or a list; `.`
/// (the project root, already a search path) is dropped. Returns paths verbatim
/// (relative to the project root) for ty to resolve. Best-effort: any read/parse
/// failure yields an empty list rather than erroring the whole query.
fn pytest_pythonpath(project_root: &str) -> Vec<String> {
    let path = std::path::Path::new(project_root).join("pyproject.toml");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    // Top-level document is a table; `Value`'s `FromStr` parses a single value.
    let Ok(doc) = text.parse::<toml::Table>() else {
        return Vec::new();
    };
    let pp = doc
        .get("tool")
        .and_then(|t| t.get("pytest"))
        .and_then(|p| p.get("ini_options"))
        .and_then(|i| i.get("pythonpath"));
    let mut out = Vec::new();
    match pp {
        Some(toml::Value::String(s)) => out.push(s.clone()),
        Some(toml::Value::Array(arr)) => {
            out.extend(arr.iter().filter_map(|v| v.as_str().map(str::to_owned)));
        }
        _ => {}
    }
    out.retain(|p| p != "." && p != "./" && !p.is_empty());
    out
}

/// Lowercased tag for a call-graph node's symbol kind. Constructors read as
/// methods (a method on a class); anything non-callable shouldn't reach here.
fn symbol_kind(kind: SymbolKind) -> &'static str {
    match kind {
        SymbolKind::Class => "class",
        SymbolKind::Method | SymbolKind::Constructor => "method",
        SymbolKind::Function => "function",
        SymbolKind::Property => "property",
        // A module appears as a caller for code that runs at module scope
        // (`if __name__ == "__main__": main()`).
        SymbolKind::Module => "module",
        _ => "callable",
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
