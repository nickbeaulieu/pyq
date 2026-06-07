//! The transitive call/reference graph, keyed by stable fully-qualified IDs.
//!
//! This is the foundation primitive (#10): a cross-file call graph whose nodes
//! are durable fully-qualified names (`pkg.models.User.__init__`) rather than
//! line numbers, so an agent can hold a node id across edits and re-query it
//! without re-grepping. Most of the heavier verbs (blast radius, dead code,
//! the symbol `describe` pack) are projections of this closure.
//!
//! Construction rides the existing locate-then-resolve seam: the syntactic
//! index assigns each callable a stable FQN and records its name offset; ty's
//! call hierarchy (`outgoing_calls`/`incoming_calls`) supplies the edges,
//! anchored at that same offset. A neighbour ty reports therefore maps straight
//! back to its FQN, and the traversal recurses by feeding the offset back to ty.
//! A breadth-first walk from the queried symbol yields the forward closure
//! (everything it transitively reaches) or — reversed — the set that reaches it.

use std::collections::{HashMap, HashSet, VecDeque};

use anyhow::Result;
use pyq_index::{DefKind, FileIndex};
use ruff_text_size::TextSize;

use crate::unified::{module_components, parse_query, scoped_by};
use crate::TyResolver;

/// Which way to walk the call graph.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    /// Callees: everything the symbol transitively calls (its dependency cone).
    Forward,
    /// Callers: everything that transitively calls the symbol (who reaches it).
    Reverse,
}

/// One node in a transitive closure, addressed by its stable FQN.
#[derive(Clone, Debug)]
pub struct GraphNode {
    /// The durable fully-qualified id (`pkg.models.User.__init__`).
    pub fqn: String,
    /// Where it currently lives — re-derivable, unlike `fqn`, after edits.
    pub path: String,
    pub line: usize,
    pub col: usize,
    /// `"function"`, `"method"`, `"class"`, … (from ty's symbol kind).
    pub kind: &'static str,
    /// Hops from the queried root (1 = a direct neighbour).
    pub depth: usize,
    /// The FQN of the node through which this one was first reached — the
    /// breadth-first tree edge, enough to reconstruct a path back to a root.
    pub via: String,
}

/// The result of a closure query.
#[derive(Clone, Debug)]
pub struct Closure {
    /// The FQN(s) the queried symbol resolved to (the traversal roots). Empty
    /// when the symbol names no function or class — distinct from "found, but
    /// nothing reaches/reachable" (a real leaf), which is roots-non-empty,
    /// nodes-empty.
    pub roots: Vec<String>,
    /// Reached nodes, deduped by FQN, ordered by (depth, fqn).
    pub nodes: Vec<GraphNode>,
}

/// A transitive call graph over one project tree.
pub struct CallGraph {
    ty: TyResolver,
    files: Vec<FileIndex>,
    /// (path, name offset) → FQN, for mapping a ty neighbour back to its id.
    fqn_by_anchor: HashMap<(String, u32), String>,
    /// Leaf name → every syntactic occurrence (bare-name use or `import`
    /// binding) of it, `(path, offset)`. The reverse closure anchors here:
    /// ty's `incoming_calls` from a *definition* misses callers that reach the
    /// symbol through an import, but from any use that resolves to it, it
    /// returns the full caller set — the sweep-the-occurrences discipline the
    /// `callers`/`refs` verbs already use.
    occurrences_by_name: HashMap<String, Vec<(String, u32)>>,
}

impl CallGraph {
    /// Build a graph over `root`/`scope` (which configure ty exactly as in
    /// [`TyResolver::new`]); `files` is the same walk's parsed facts, used to
    /// assign FQNs and locate roots.
    pub fn new(root: &str, files: Vec<FileIndex>, scope: HashSet<std::path::PathBuf>) -> Result<Self> {
        let ty = TyResolver::new(root, scope)?;
        let mut fqn_by_anchor = HashMap::new();
        let mut occurrences_by_name: HashMap<String, Vec<(String, u32)>> = HashMap::new();
        for f in &files {
            for d in &f.defs {
                match d.kind {
                    // Only callables are graph nodes; an import binding re-binds
                    // a name elsewhere and isn't a definition site.
                    DefKind::Function | DefKind::Class => {
                        let fqn = fqn_of(&f.path, &d.container, &d.name);
                        fqn_by_anchor.insert((f.path.clone(), d.offset), fqn);
                    }
                    // An `import` binding is a cross-module occurrence ty can
                    // anchor a reverse walk on (the def site alone can't).
                    DefKind::Import => occurrences_by_name
                        .entry(d.name.clone())
                        .or_default()
                        .push((f.path.clone(), d.offset)),
                    DefKind::Variable => {}
                }
            }
            for r in &f.refs {
                occurrences_by_name
                    .entry(r.name.clone())
                    .or_default()
                    .push((f.path.clone(), r.offset));
            }
        }
        Ok(CallGraph {
            ty,
            files,
            fqn_by_anchor,
            occurrences_by_name,
        })
    }

    /// The transitive closure of `symbol` in `dir`, optionally capped at
    /// `max_depth` hops (`None` = unbounded — the visited set still bounds it to
    /// the project's reachable size, cycles included).
    pub fn closure(&self, symbol: &str, dir: Direction, max_depth: Option<usize>) -> Closure {
        let starts = self.start_anchors(symbol);
        let roots: Vec<String> = starts.iter().map(|(_, _, fqn)| fqn.clone()).collect();

        // Seed visited with the roots so a root reached again (recursion) is not
        // re-emitted as its own neighbour.
        let mut visited: HashSet<String> = roots.iter().cloned().collect();
        let mut nodes: Vec<GraphNode> = Vec::new();
        let mut queue: VecDeque<(String, u32, String, usize)> = starts
            .into_iter()
            .map(|(path, offset, fqn)| (path, offset, fqn, 0))
            .collect();

        while let Some((path, offset, fqn, depth)) = queue.pop_front() {
            if max_depth.is_some_and(|max| depth >= max) {
                continue;
            }
            let neighbours = self.neighbours(&path, offset, &fqn, dir);
            for nb in neighbours {
                let nfqn = self
                    .fqn_by_anchor
                    .get(&(nb.path.clone(), nb.offset))
                    .cloned()
                    .unwrap_or_else(|| fallback_fqn(&nb.path, &nb.name, nb.kind));
                if !visited.insert(nfqn.clone()) {
                    continue;
                }
                nodes.push(GraphNode {
                    fqn: nfqn.clone(),
                    path: nb.path.clone(),
                    line: nb.line,
                    col: nb.col,
                    kind: nb.kind,
                    depth: depth + 1,
                    via: fqn.clone(),
                });
                queue.push_back((nb.path, nb.offset, nfqn, depth + 1));
            }
        }

        nodes.sort_by(|a, b| (a.depth, &a.fqn).cmp(&(b.depth, &b.fqn)));
        Closure { roots, nodes }
    }

    /// Whole-project caller index: for every first-party callable, the set of
    /// distinct first-party caller FQNs that statically call it. One forward
    /// `outgoing_at` sweep per node — the same cost as building the graph once —
    /// accumulating the *reverse* of each resolved call edge (a callee gains its
    /// caller). Self-edges from recursion are excluded (a function calling
    /// itself isn't "used elsewhere"); only edges whose callee maps back to a
    /// known def anchor count, so third-party callees are naturally dropped.
    /// The basis for ranking the repo's most-reused internal helpers
    /// (`canonical`). Inherits the call graph's dynamic-dispatch blind spot, so
    /// a helper reached only via attribute/framework dispatch is undercounted.
    pub fn caller_index(&self) -> HashMap<String, HashSet<String>> {
        let mut callers: HashMap<String, HashSet<String>> = HashMap::new();
        for ((path, offset), caller_fqn) in &self.fqn_by_anchor {
            for nb in self.ty.outgoing_at(path, TextSize::from(*offset)) {
                if let Some(callee_fqn) = self.fqn_by_anchor.get(&(nb.path.clone(), nb.offset)) {
                    if callee_fqn != caller_fqn {
                        callers
                            .entry(callee_fqn.clone())
                            .or_default()
                            .insert(caller_fqn.clone());
                    }
                }
            }
        }
        callers
    }

    /// The immediate base classes of the class at `(path, offset)` — see
    /// [`TyResolver::supertypes_at`]. Lets a caller build the inheritance graph.
    pub fn supertypes_at(&self, path: &str, offset: u32) -> Vec<crate::SuperClass> {
        self.ty.supertypes_at(path, offset)
    }

    /// Whether `member` is a top-level name of the module the binding at
    /// `(path, offset)` resolves to — see [`TyResolver::module_member`].
    pub fn module_member(&self, path: &str, offset: u32, member: &str) -> crate::MemberCheck {
        self.ty.module_member(path, offset, member)
    }

    /// Resolve the use at `(path, offset)` to its definition's `(path, name
    /// offset)` anchor, following imports — `None` if it doesn't resolve to an
    /// in-scope project def. Lets a caller seed reachability from a *use site*
    /// (e.g. a module-scope reference to a callable) rather than a def.
    pub fn resolve_anchor(&self, path: &str, offset: u32) -> Option<(String, u32)> {
        self.ty.resolve_def_at(path, TextSize::from(offset))
    }

    /// Every FQN forward-reachable from any of `seeds` (each a `(path, name
    /// offset)` anchor), the seeds themselves included. One multi-source BFS —
    /// for whole-program reachability (dead-code) where per-root attribution and
    /// depth aren't needed, so it returns just the reachable set and never
    /// re-walks an overlapping subgraph.
    ///
    /// `extra_edges` adds non-call successors: when the walk reaches a node whose
    /// FQN is a key, the mapped anchors are enqueued too. Dead-code uses this for
    /// override edges (a reached base method pulls its overrides in), folding the
    /// polymorphic propagation into the single BFS instead of a re-run fixpoint.
    pub fn reachable_from(
        &self,
        seeds: &[(String, u32)],
        extra_edges: &HashMap<String, Vec<(String, u32)>>,
    ) -> HashSet<String> {
        let mut visited: HashSet<String> = HashSet::new();
        let mut queue: VecDeque<(String, u32, String)> = VecDeque::new();
        let enqueue =
            |path: String, offset: u32, fqn: String, visited: &mut HashSet<String>, queue: &mut VecDeque<_>| {
                if visited.insert(fqn.clone()) {
                    queue.push_back((path, offset, fqn));
                }
            };
        for (path, offset) in seeds {
            let fqn = self
                .fqn_by_anchor
                .get(&(path.clone(), *offset))
                .cloned()
                .unwrap_or_else(|| scope_fqn(path, &[]));
            enqueue(path.clone(), *offset, fqn, &mut visited, &mut queue);
        }
        while let Some((path, offset, fqn)) = queue.pop_front() {
            for nb in self.neighbours(&path, offset, &fqn, Direction::Forward) {
                let nfqn = self
                    .fqn_by_anchor
                    .get(&(nb.path.clone(), nb.offset))
                    .cloned()
                    .unwrap_or_else(|| fallback_fqn(&nb.path, &nb.name, nb.kind));
                enqueue(nb.path, nb.offset, nfqn, &mut visited, &mut queue);
            }
            // Override edges: a reached base method makes its overrides reachable.
            if let Some(overrides) = extra_edges.get(&fqn) {
                for (opath, ooffset) in overrides {
                    let ofqn = self
                        .fqn_by_anchor
                        .get(&(opath.clone(), *ooffset))
                        .cloned()
                        .unwrap_or_else(|| scope_fqn(opath, &[]));
                    enqueue(opath.clone(), *ooffset, ofqn, &mut visited, &mut queue);
                }
            }
        }
        visited
    }

    /// The direct call-graph neighbours of the node at `(path, offset)`.
    ///
    /// Forward is straightforward — ty's outgoing calls read the def's own body,
    /// so the def anchor is complete. Reverse needs the sweep: `incoming_calls`
    /// from a definition misses callers that reach it through an import, so we
    /// also anchor at every syntactic occurrence of the node's leaf name that
    /// `goto_definition`-resolves back to *this* node (which keeps same-named
    /// symbols from merging). Anchoring `incoming_calls` at any one such use
    /// returns the symbol's full caller set; the union over occurrences is
    /// defensive against ty surfacing different callers from different anchors.
    fn neighbours(&self, path: &str, offset: u32, fqn: &str, dir: Direction) -> Vec<crate::Neighbor> {
        if dir == Direction::Forward {
            return self.ty.outgoing_at(path, TextSize::from(offset));
        }
        let mut seen: HashSet<(String, u32)> = HashSet::new();
        let mut out = Vec::new();
        let mut take = |nbs: Vec<crate::Neighbor>, out: &mut Vec<crate::Neighbor>| {
            for nb in nbs {
                if seen.insert((nb.path.clone(), nb.offset)) {
                    out.push(nb);
                }
            }
        };
        take(self.ty.incoming_at(path, TextSize::from(offset)), &mut out);
        let leaf = fqn.rsplit('.').next().unwrap_or(fqn);
        if let Some(occurrences) = self.occurrences_by_name.get(leaf) {
            for (opath, ooff) in occurrences {
                if opath == path && *ooff == offset {
                    continue; // the def anchor, already swept
                }
                if self.ty.resolve_def_at(opath, TextSize::from(*ooff))
                    == Some((path.to_string(), offset))
                {
                    take(self.ty.incoming_at(opath, TextSize::from(*ooff)), &mut out);
                }
            }
        }
        out
    }

    /// Resolve `symbol` (bare, qualified, or a full FQN) to its traversal roots:
    /// every function/class def whose name is the leaf and whose scope the
    /// qualifier (if any) suffixes — `(path, name offset, fqn)` apiece.
    fn start_anchors(&self, symbol: &str) -> Vec<(String, u32, String)> {
        let (leaf, qualifier) = parse_query(symbol);
        let mut out = Vec::new();
        for f in &self.files {
            for d in &f.defs {
                if d.name == leaf
                    && matches!(d.kind, DefKind::Function | DefKind::Class)
                    && scoped_by(&qualifier, &f.path, &d.container)
                {
                    out.push((
                        f.path.clone(),
                        d.offset,
                        fqn_of(&f.path, &d.container, &d.name),
                    ));
                }
            }
        }
        out
    }
}

/// The stable fully-qualified id of a def: module components + enclosing scopes
/// + name (`pkg/models.py`, `["User"]`, `__init__` → `pkg.models.User.__init__`).
fn fqn_of(path: &str, container: &[String], name: &str) -> String {
    let mut scope: Vec<String> = container.to_vec();
    scope.push(name.to_string());
    scope_fqn(path, &scope)
}

/// The fully-qualified id for a *scope path* in a file: the module components
/// followed by the enclosing class/function names. `pkg/models.py` + `["User",
/// "__init__"]` → `pkg.models.User.__init__`; an empty scope is the module
/// itself. The canonical mapping from "(file, scope)" to a graph node id —
/// shared so other verbs (e.g. `effects`) attribute facts to the same ids the
/// call graph uses.
pub fn scope_fqn(path: &str, scope: &[String]) -> String {
    let mut parts: Vec<String> = module_components(path)
        .into_iter()
        .map(str::to_string)
        .collect();
    parts.extend(scope.iter().cloned());
    parts.join(".")
}

/// FQN for an in-scope callable the syntactic index didn't capture as a def
/// (e.g. a comprehension/lambda ty still names): module path + the ty name. A
/// module caller (module-scope code) *is* the module, so its FQN is just the
/// module path — not the name repeated (`app`, not `app.app`).
fn fallback_fqn(path: &str, name: &str, kind: &str) -> String {
    let mut parts: Vec<String> = module_components(path)
        .into_iter()
        .map(str::to_string)
        .collect();
    if kind != "module" {
        parts.push(name.to_string());
    }
    parts.join(".")
}
