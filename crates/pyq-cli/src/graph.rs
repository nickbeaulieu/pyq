//! The project import graph, built syntactically from per-file [`ImportStmt`]s.
//!
//! Files map to dotted module names; each import statement resolves to one or
//! more target modules (relative imports resolved against the importer's
//! package). Targets that match a project module — exactly or as a package
//! prefix — are *internal*; everything else (`os`, `click`, …) is *external*.
//! Cycles are the non-trivial strongly-connected components over internal edges.

use pyq_index::{FileIndex, ImportContext, Pos};
use std::collections::{BTreeMap, BTreeSet};

/// One importer → target edge.
pub struct Edge {
    pub importer: String,
    pub importer_file: String,
    pub target: String,
    pub pos: Pos,
    pub internal: bool,
    /// When the import runs — only `TopLevel` edges count toward cycles.
    pub context: ImportContext,
}

pub struct Graph {
    pub edges: Vec<Edge>,
    /// Project module name → its file path.
    module_file: BTreeMap<String, String>,
}

impl Graph {
    pub fn build(files: &[FileIndex]) -> Self {
        // First pass: every project module and its file.
        let mut module_file = BTreeMap::new();
        let mut is_pkg = BTreeMap::new();
        for f in files {
            let (module, pkg) = module_of(&f.path);
            is_pkg.insert(module.clone(), pkg);
            module_file.insert(module, f.path.clone());
        }
        let modules: BTreeSet<&String> = module_file.keys().collect();

        // Second pass: resolve every import statement to target module(s).
        let mut edges = Vec::new();
        for f in files {
            let (importer, pkg) = module_of(&f.path);
            for stmt in &f.imports {
                let (primary, submodules) = resolve_targets(&importer, pkg, stmt);
                for target in primary {
                    // The literal import string (`main.models`) may differ from
                    // the file-derived module id (`alice.main.models`) when the
                    // project is rooted below the repo (Django/src/pythonpath).
                    // Canonicalize to the file-derived id so forward and reverse
                    // deps key on the *same* identity and actually compose.
                    let target = canonicalize_target(&target, &module_file);
                    let internal = is_internal(&target, &modules);
                    edges.push(Edge {
                        importer: importer.clone(),
                        importer_file: f.path.clone(),
                        target,
                        pos: stmt.pos,
                        internal,
                        context: stmt.context,
                    });
                }
                // `from M import name` where `M.name` is itself a project module
                // is a precise edge to that submodule (e.g. a package re-import
                // forming a cycle). Keep only the ones that resolve.
                for sub in submodules {
                    if module_file.contains_key(&sub) {
                        edges.push(Edge {
                            importer: importer.clone(),
                            importer_file: f.path.clone(),
                            target: sub,
                            pos: stmt.pos,
                            internal: true,
                            context: stmt.context,
                        });
                    }
                }
            }
        }
        Graph { edges, module_file }
    }

    pub fn file_of(&self, module: &str) -> Option<&str> {
        self.module_file.get(module).map(String::as_str)
    }

    /// Map a queried module name (any accepted spelling) to its canonical
    /// file-derived identity — so `imports main.models` and the printed
    /// `alice.main.models` resolve to the same node.
    pub fn resolve_module(&self, name: &str) -> String {
        canonicalize_target(name, &self.module_file)
    }

    /// Whether `module` exists in the graph at all — a project module file, or a
    /// package/module that appears as an importer or target. Lets a query
    /// distinguish "found, no edges" (real leaf) from "not found" (typo).
    pub fn knows(&self, module: &str) -> bool {
        self.module_file.contains_key(module)
            || self
                .edges
                .iter()
                .any(|e| e.importer == module || e.target == module)
    }

    /// Internal modules forming the nodes of the dependency graph. Only
    /// `TopLevel` edges (run at import time) count — `TYPE_CHECKING` and
    /// deferred/function-local imports are exactly how code *breaks* runtime
    /// cycles, so they must not be reported as cycles.
    fn internal_adjacency(&self) -> BTreeMap<&str, BTreeSet<&str>> {
        let mut adj: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
        for m in self.module_file.keys() {
            adj.entry(m).or_default();
        }
        for e in &self.edges {
            // Only import-time edges between actual module nodes count.
            if e.context == ImportContext::TopLevel && self.module_file.contains_key(&e.target) {
                adj.entry(e.importer.as_str())
                    .or_default()
                    .insert(e.target.as_str());
            }
        }
        adj
    }

    /// Import cycles, each as an ordered path (not repeating the closing node),
    /// computed as the non-trivial strongly-connected components over
    /// import-time edges, with one concrete cycle extracted per SCC so the
    /// output names an edge to cut.
    pub fn cycles(&self) -> Vec<Vec<String>> {
        let adj = self.internal_adjacency();
        tarjan_scc(&adj)
            .into_iter()
            .filter(|scc| scc.len() > 1 || self_loop(&adj, scc))
            .filter_map(|scc| {
                let set: BTreeSet<&str> = scc.iter().copied().collect();
                extract_cycle(&adj, &set).map(|c| c.into_iter().map(str::to_string).collect())
            })
            .collect()
    }
}

fn self_loop(adj: &BTreeMap<&str, BTreeSet<&str>>, scc: &[&str]) -> bool {
    scc.len() == 1 && adj.get(scc[0]).is_some_and(|t| t.contains(scc[0]))
}

/// Extract one concrete cycle (ordered, not repeating the closing node) from a
/// strongly-connected node set — a DFS following intra-SCC edges until it
/// revisits a node already on the current path.
fn extract_cycle<'a>(
    adj: &BTreeMap<&'a str, BTreeSet<&'a str>>,
    scc: &BTreeSet<&'a str>,
) -> Option<Vec<&'a str>> {
    let start = *scc.iter().next()?;
    if scc.len() == 1 {
        return Some(vec![start]); // self-loop
    }
    let mut path: Vec<&str> = Vec::new();
    let mut on_path: BTreeSet<&str> = BTreeSet::new();
    let mut stack = vec![(start, false)];
    while let Some((node, backtrack)) = stack.pop() {
        if backtrack {
            on_path.remove(node);
            path.pop();
            continue;
        }
        path.push(node);
        on_path.insert(node);
        stack.push((node, true));
        if let Some(succs) = adj.get(node) {
            for &w in succs.iter().filter(|w| scc.contains(*w)) {
                if on_path.contains(w) {
                    let pos = path.iter().position(|&n| n == w).unwrap();
                    return Some(path[pos..].to_vec());
                }
                stack.push((w, false));
            }
        }
    }
    None
}

/// Map an import target to the canonical file-derived module id: the exact
/// match if one exists, else the *unique* project module whose name ends with
/// `.<target>` (the source-root case, where code imports `main.models` but the
/// file is `alice/main/models.py`). Ambiguous or unmatched → unchanged.
fn canonicalize_target(target: &str, module_file: &BTreeMap<String, String>) -> String {
    if module_file.contains_key(target) {
        return target.to_string();
    }
    let suffix = format!(".{target}");
    let mut matches = module_file.keys().filter(|m| m.ends_with(suffix.as_str()));
    match (matches.next(), matches.next()) {
        (Some(only), None) => only.clone(),
        _ => target.to_string(),
    }
}

/// A target is internal if it names a project module exactly, or is a package
/// whose submodules are project modules (`import pkg` where `pkg.models` exists).
fn is_internal(target: &str, modules: &BTreeSet<&String>) -> bool {
    if modules.contains(&target.to_string()) {
        return true;
    }
    let prefix = format!("{target}.");
    modules.iter().any(|m| m.starts_with(&prefix))
}

/// Normalize a user-supplied argument (a module name or a file path) to a
/// dotted module name. `pkg/models.py` → `pkg.models`; `pkg.models` is kept.
pub fn normalize_query(arg: &str) -> String {
    if arg.ends_with(".py") || arg.ends_with(".pyi") || arg.contains('/') || arg.contains('\\') {
        module_of(arg).0
    } else {
        arg.trim_matches('.').to_string()
    }
}

/// Derive a module's dotted name from its project-relative path, and whether it
/// is a package (`__init__`). `pkg/models.py` → (`pkg.models`, false);
/// `pkg/__init__.py` → (`pkg`, true).
fn module_of(path: &str) -> (String, bool) {
    let stem = path
        .strip_suffix(".pyi")
        .or_else(|| path.strip_suffix(".py"))
        .unwrap_or(path);
    let mut parts: Vec<&str> = stem.split(['/', '\\']).filter(|p| !p.is_empty()).collect();
    let pkg = parts.last() == Some(&"__init__");
    if pkg {
        parts.pop();
    }
    (parts.join("."), pkg)
}

/// Resolve one import statement to `(primary targets, submodule candidates)`.
/// Primary targets are the module(s) imported from; submodule candidates are
/// `<module>.<name>` guesses the caller filters against real project modules.
fn resolve_targets(
    importer: &str,
    importer_is_pkg: bool,
    stmt: &pyq_index::ImportStmt,
) -> (Vec<String>, Vec<String>) {
    if stmt.level == 0 {
        // Absolute: `import a.b`, or `from a.b import x, y`.
        if stmt.module.is_empty() {
            return (Vec::new(), Vec::new());
        }
        let subs = stmt
            .names
            .iter()
            .map(|n| format!("{}.{}", stmt.module, n))
            .collect();
        return (vec![stmt.module.clone()], subs);
    }

    // Relative: anchor at the importer's package, then ascend `level - 1`.
    let mut base: Vec<String> = importer.split('.').map(String::from).collect();
    if !importer_is_pkg {
        base.pop(); // a module's anchor is its containing package
    }
    for _ in 0..stmt.level.saturating_sub(1) {
        base.pop();
    }

    if !stmt.module.is_empty() {
        base.extend(stmt.module.split('.').map(String::from));
        let target = base.join(".");
        let subs = stmt.names.iter().map(|n| format!("{target}.{n}")).collect();
        (vec![target], subs)
    } else {
        // `from . import a, b` → the targets are the submodules `<base>.a`, etc.
        let prim = stmt
            .names
            .iter()
            .map(|n| {
                let mut t = base.clone();
                t.push(n.clone());
                t.join(".")
            })
            .collect();
        (prim, Vec::new())
    }
}

/// Tarjan's strongly-connected components. Deterministic (inputs are ordered).
fn tarjan_scc<'a>(adj: &BTreeMap<&'a str, BTreeSet<&'a str>>) -> Vec<Vec<&'a str>> {
    struct State<'a> {
        index: BTreeMap<&'a str, usize>,
        low: BTreeMap<&'a str, usize>,
        on_stack: BTreeSet<&'a str>,
        stack: Vec<&'a str>,
        next: usize,
        out: Vec<Vec<&'a str>>,
    }

    fn strongconnect<'a>(
        v: &'a str,
        adj: &BTreeMap<&'a str, BTreeSet<&'a str>>,
        s: &mut State<'a>,
    ) {
        s.index.insert(v, s.next);
        s.low.insert(v, s.next);
        s.next += 1;
        s.stack.push(v);
        s.on_stack.insert(v);

        if let Some(succs) = adj.get(v) {
            for &w in succs {
                if !s.index.contains_key(w) {
                    strongconnect(w, adj, s);
                    let lw = s.low[w];
                    let lv = s.low[v];
                    s.low.insert(v, lv.min(lw));
                } else if s.on_stack.contains(w) {
                    let iw = s.index[w];
                    let lv = s.low[v];
                    s.low.insert(v, lv.min(iw));
                }
            }
        }

        if s.low[v] == s.index[v] {
            let mut scc = Vec::new();
            while let Some(w) = s.stack.pop() {
                s.on_stack.remove(w);
                scc.push(w);
                if w == v {
                    break;
                }
            }
            s.out.push(scc);
        }
    }

    let mut s = State {
        index: BTreeMap::new(),
        low: BTreeMap::new(),
        on_stack: BTreeSet::new(),
        stack: Vec::new(),
        next: 0,
        out: Vec::new(),
    };
    for &v in adj.keys() {
        if !s.index.contains_key(v) {
            strongconnect(v, adj, &mut s);
        }
    }
    s.out
}
