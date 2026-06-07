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
use serde::{Deserialize, Serialize};

use crate::unified::{module_components, parse_query, scoped_by};
use crate::{MemberCheck, Neighbor, SuperClass, TyResolver};

/// Which sub-phase of [`CallGraph::record_with_progress`] a progress tick
/// reports — lets a caller render a labelled bar without knowing the internals.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecordPhase {
    /// Recording call edges (`outgoing` + the transposed `incoming`) — the bulk.
    Edges,
    /// Resolving syntactic occurrences (`refs` + import bindings).
    Resolve,
}

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
///
/// The structural facts (FQNs, anchors, occurrences) are derived purely from the
/// syntactic index; the *edges* come from a [`CallGraphTy`] source, which is
/// either live ty ([`TyResolver`]) or a [`ReplayTy`] replaying a recorded
/// [`GraphRecording`]. A replayed graph answers every traversal without ever
/// constructing ty — the warm path of the analysis cache (#38.3).
pub struct CallGraph {
    ty: Box<dyn CallGraphTy>,
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
        Ok(Self::assemble(Box::new(ty), files))
    }

    /// A [`CallGraph`] whose edges come from a recorded [`GraphRecording`]
    /// instead of live ty — the warm path. The structural maps are rebuilt from
    /// `files` (a pure, ty-free derivation), so a replayed graph is byte-for-byte
    /// equivalent to a live one for every recorded query.
    pub fn replay(files: Vec<FileIndex>, recording: GraphRecording) -> Self {
        Self::assemble(Box::new(ReplayTy { rec: recording }), files)
    }

    /// Build the structural maps (FQN-by-anchor, occurrences-by-name) from the
    /// syntactic index and pair them with an edge source. Shared by [`new`] and
    /// [`replay`] so both graphs are structurally identical.
    fn assemble(ty: Box<dyn CallGraphTy>, files: Vec<FileIndex>) -> Self {
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
        CallGraph {
            ty,
            files,
            fqn_by_anchor,
            occurrences_by_name,
        }
    }

    /// Record the ty-query surface for replay (#38.3), so a [`ReplayTy`] answers
    /// every traversal without ty. See [`Self::record_with_progress`]; this is the
    /// silent form.
    pub fn record(&self) -> GraphRecording {
        self.record_with_progress(&|_, _| {})
    }

    /// Record the ty-query surface, reporting coarse progress as it goes.
    ///
    /// The naive recording asked ty for the *callers* of every anchor — an
    /// `incoming_calls` per node, each a whole-project reference scan — which on a
    /// large tree is O(nodes × files) and dominates indexing. Instead we record
    /// only the cheap, **local** facts and derive the reverse map:
    /// 1. `outgoing` for every def anchor and every node forward-reachable from
    ///    one (a worklist over callees) — each call reads only that def's body;
    /// 2. `incoming` as the **transpose** of those edges (`A → B` ⇒ `B` gains
    ///    caller `A`). The transpose is in fact *more* faithful than a
    ///    def-anchored `incoming_calls`: an aliased call (`from m import f as g;
    ///    g()`) is resolved by the caller's own `outgoing`, so the caller is
    ///    captured without the occurrence-sweep that `incoming_calls` needed.
    ///    Module-scope callers (import-time code, in no def's body) are recovered
    ///    from the parsed `is_call`/`module_scope` references in pass 3;
    /// 3. `supertypes` per class, and `resolve` per syntactic occurrence.
    pub fn record_with_progress(&self, progress: &dyn Fn(RecordPhase, usize)) -> GraphRecording {
        self.record_inner(None, progress)
    }

    /// Re-record only the files affected by a change, reusing `prev` for the rest
    /// (#38.5). `changed` is the set of files whose parse differs from when `prev`
    /// was recorded; `import_adj` is the project's undirected file-level import
    /// adjacency. The *affected* set — what must be re-queried — is the import
    /// component of `changed` (every cross-file ty dependency flows along an
    /// import edge) plus any prior entry whose recorded value points into a
    /// changed file (the safety net for deletions / import-graph gaps). Entries
    /// outside it are copied from `prev` verbatim, so the result is byte-identical
    /// to a from-scratch record but skips ty for the untouched majority.
    pub fn record_incremental(
        &self,
        prev: GraphRecording,
        changed: &HashSet<String>,
        import_adj: &HashMap<String, Vec<String>>,
        progress: &dyn Fn(RecordPhase, usize),
    ) -> GraphRecording {
        let affected = compute_affected(&prev, changed, import_adj);
        self.record_inner(Some((&prev, &affected)), progress)
    }

    /// The recording engine. `reuse = Some((prev, affected))` copies every `prev`
    /// entry anchored outside `affected` and re-records only the `affected` files;
    /// `None` records the whole tree.
    fn record_inner(
        &self,
        reuse: Option<(&GraphRecording, &HashSet<String>)>,
        progress: &dyn Fn(RecordPhase, usize),
    ) -> GraphRecording {
        let mut rec = GraphRecording::default();

        // Incremental: carry over every entry anchored in an unaffected file. The
        // import-component property guarantees an affected callee's callers are
        // themselves affected, so a reused `incoming` entry (unaffected callee) is
        // complete, and the rebuilt ones below cover the rest.
        if let Some((prev, affected)) = reuse {
            let keep = |k: &(String, u32)| !affected.contains(&k.0);
            rec.outgoing.extend(prev.outgoing.iter().filter(|(k, _)| keep(k)).map(|(k, v)| (k.clone(), v.clone())));
            rec.incoming.extend(prev.incoming.iter().filter(|(k, _)| keep(k)).map(|(k, v)| (k.clone(), v.clone())));
            rec.resolve.extend(prev.resolve.iter().filter(|(k, _)| keep(k)).map(|(k, v)| (k.clone(), v.clone())));
            rec.supertypes.extend(prev.supertypes.iter().filter(|(k, _)| keep(k)).map(|(k, v)| (k.clone(), v.clone())));
        }
        // Whether this file's anchors should be (re-)recorded now.
        let want = |path: &str| reuse.is_none_or(|(_, affected)| affected.contains(path));

        // Scope paths (`container + name`) of every class, so a function def can
        // be tagged `method` vs `function` exactly as ty's symbol kind would —
        // it's a method iff its immediate enclosing scope is one of these.
        let class_scopes: HashSet<Vec<String>> = self
            .files
            .iter()
            .flat_map(|f| f.defs.iter().filter(|d| d.kind == DefKind::Class))
            .map(|d| {
                let mut s = d.container.clone();
                s.push(d.name.clone());
                s
            })
            .collect();

        // (path, offset) → how to render this node when it appears as a *caller*
        // in the transposed incoming map. Seeded from def anchors; filled for any
        // forward-discovered node (e.g. a lambda ty names) when first seen.
        let mut node_meta: HashMap<(String, u32), NeighborData> = HashMap::new();
        let mut work: VecDeque<(String, u32)> = VecDeque::new();
        for f in &self.files {
            if !want(&f.path) {
                continue;
            }
            for d in &f.defs {
                if matches!(d.kind, DefKind::Function | DefKind::Class) {
                    let key = (f.path.clone(), d.offset);
                    node_meta.insert(key.clone(), def_neighbor(f, d, &class_scopes));
                    work.push_back(key);
                }
            }
        }
        let mut seen: HashSet<(String, u32)> = node_meta.keys().cloned().collect();

        // Pass 1: for every node, record `outgoing` (ty) and accumulate
        // `incoming` from the *union* of two sources, so the reverse map is a
        // strict superset of either alone — no false callers (both ty-verified),
        // no missed ones:
        //   - ty's own `incoming_at` — the direct/property/lambda callers ty
        //     attributes, including caller nodes (lambdas) the transpose can't
        //     name; and
        //   - the **transpose** of `outgoing` — the aliased callers ty's
        //     def-anchored `incoming_at` misses (`from m import f as g; g()` is
        //     resolved by the caller's own `outgoing`).
        // The worklist runs in breadth-first **waves**: each wave's per-anchor ty
        // queries (the expensive `incoming_calls`) run in parallel via
        // `edges_batch`; the results are folded in sequentially (cheap). Wave 1 is
        // every def anchor — the bulk — so nearly all the work parallelizes;
        // later waves are the few non-def nodes ty surfaces (lambdas).
        let mut frontier: Vec<(String, u32)> = work.into_iter().collect();
        let mut done = 0usize;
        while !frontier.is_empty() {
            let mut next: Vec<(String, u32)> = Vec::new();
            // Chunk the wave so the parallel batch is sized for good core use
            // while the bar still advances between chunks (a single batch over the
            // whole wave would block, then jump). 256 ≫ core count, so each chunk
            // saturates the pool.
            for chunk in frontier.chunks(256) {
                for ((path, offset), (out, inc)) in chunk.iter().zip(self.ty.edges_batch(chunk)) {
                    let caller = node_meta
                        .get(&(path.clone(), *offset))
                        .cloned()
                        .unwrap_or_else(|| module_neighbor(path));
                    for nb in &out {
                        let key = (nb.path.clone(), nb.offset);
                        node_meta.entry(key.clone()).or_insert_with(|| neighbor_data(nb));
                        if seen.insert(key.clone()) {
                            next.push(key.clone());
                        }
                        // transpose: the callee gains the current node as a caller.
                        rec.incoming.entry(key).or_default().push(caller.clone());
                    }
                    // ty's direct view of this node's callers (property/lambda/direct).
                    for nb in &inc {
                        let key = (nb.path.clone(), nb.offset);
                        node_meta.entry(key.clone()).or_insert_with(|| neighbor_data(nb));
                        if seen.insert(key.clone()) {
                            next.push(key.clone());
                        }
                    }
                    rec.incoming.entry((path.clone(), *offset)).or_default().extend(to_data(&inc));
                    rec.outgoing.insert((path.clone(), *offset), to_data(&out));
                    done += 1;
                }
                progress(RecordPhase::Edges, done);
            }
            frontier = next;
        }

        // Pass 2: supertypes per class.
        for f in &self.files {
            if !want(&f.path) {
                continue;
            }
            for d in &f.defs {
                if d.kind == DefKind::Class {
                    rec.supertypes.insert(
                        (f.path.clone(), d.offset),
                        self.ty.supertypes_at(&f.path, d.offset),
                    );
                }
            }
        }

        // Pass 3: resolve every occurrence (refs + import bindings) in parallel,
        // then recover module-scope caller edges (a `name()` at import scope, in
        // no def's body, that the transpose can't see) by attributing the call to
        // its module node. `module_call` flags the refs that feed that recovery.
        let mut occ: Vec<(String, u32)> = Vec::new();
        let mut module_call: Vec<bool> = Vec::new();
        for f in &self.files {
            if !want(&f.path) {
                continue;
            }
            for r in &f.refs {
                let key = (f.path.clone(), r.offset);
                if !rec.resolve.contains_key(&key) {
                    occ.push(key);
                    module_call.push(r.is_call && r.module_scope);
                }
            }
            for d in f.defs.iter().filter(|d| d.kind == DefKind::Import) {
                let key = (f.path.clone(), d.offset);
                if !rec.resolve.contains_key(&key) {
                    occ.push(key);
                    module_call.push(false);
                }
            }
        }
        progress(RecordPhase::Resolve, 0);
        let mut done3 = 0usize;
        let flags = module_call;
        for (chunk, flag_chunk) in occ.chunks(4096).zip(flags.chunks(4096)) {
            for ((key, is_module_call), res) in
                chunk.iter().zip(flag_chunk).zip(self.ty.resolve_batch(chunk))
            {
                if *is_module_call {
                    if let Some(callee) = res.clone() {
                        rec.incoming.entry(callee).or_default().push(module_neighbor(&key.0));
                    }
                }
                rec.resolve.insert(key.clone(), res);
                done3 += 1;
            }
            progress(RecordPhase::Resolve, done3);
        }

        // A node may call the same callee more than once; the reverse traversal
        // dedups by FQN downstream, but keep the recording tight.
        for nbs in rec.incoming.values_mut() {
            let mut seen = HashSet::new();
            nbs.retain(|n| seen.insert((n.path.clone(), n.offset)));
        }

        rec
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
            for nb in self.ty.outgoing_at(path, *offset) {
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
        self.ty.resolve_def_at(path, offset)
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
            return self.ty.outgoing_at(path, offset);
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
        take(self.ty.incoming_at(path, offset), &mut out);
        let leaf = fqn.rsplit('.').next().unwrap_or(fqn);
        if let Some(occurrences) = self.occurrences_by_name.get(leaf) {
            for (opath, ooff) in occurrences {
                if opath == path && *ooff == offset {
                    continue; // the def anchor, already swept
                }
                if self.ty.resolve_def_at(opath, *ooff) == Some((path.to_string(), offset)) {
                    take(self.ty.incoming_at(opath, *ooff), &mut out);
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

/// The files whose recorded entries must be re-queried after `changed` files
/// moved (#38.5): the import component of `changed` — every cross-file ty
/// dependency (a call resolving into another module, a caller, a base class)
/// flows along an import edge, so a file connected to none of the changed files
/// can't have changed semantically — plus a value-reference safety net: any
/// prior entry whose recorded callee/caller/target/base lives in a changed file,
/// which catches a *deleted* file's importers (the current import graph no longer
/// links them).
fn compute_affected(
    prev: &GraphRecording,
    changed: &HashSet<String>,
    import_adj: &HashMap<String, Vec<String>>,
) -> HashSet<String> {
    let mut affected = changed.clone();
    let mut queue: VecDeque<String> = changed.iter().cloned().collect();
    while let Some(f) = queue.pop_front() {
        if let Some(neighbours) = import_adj.get(&f) {
            for n in neighbours {
                if affected.insert(n.clone()) {
                    queue.push_back(n.clone());
                }
            }
        }
    }
    for (k, callees) in &prev.outgoing {
        if !affected.contains(&k.0) && callees.iter().any(|n| changed.contains(&n.path)) {
            affected.insert(k.0.clone());
        }
    }
    for (k, callers) in &prev.incoming {
        if !affected.contains(&k.0) && callers.iter().any(|n| changed.contains(&n.path)) {
            affected.insert(k.0.clone());
        }
    }
    for (k, target) in &prev.resolve {
        if !affected.contains(&k.0) && target.as_ref().is_some_and(|(p, _)| changed.contains(p)) {
            affected.insert(k.0.clone());
        }
    }
    for (k, supers) in &prev.supertypes {
        if !affected.contains(&k.0)
            && supers.iter().any(|s| s.anchor.as_ref().is_some_and(|(p, _)| changed.contains(p)))
        {
            affected.insert(k.0.clone());
        }
    }
    affected
}

/// The edge source a [`CallGraph`] traverses against. Implemented by live ty
/// ([`TyResolver`]) and by [`ReplayTy`] (recorded answers). Offsets are byte
/// offsets of a name; every method is keyed on `(path, offset)` so the two
/// implementations are interchangeable and a recording replays exactly.
pub trait CallGraphTy {
    /// Callables directly called by the node at `(path, offset)`.
    fn outgoing_at(&self, path: &str, offset: u32) -> Vec<Neighbor>;
    /// Callables that directly call the node at `(path, offset)`.
    fn incoming_at(&self, path: &str, offset: u32) -> Vec<Neighbor>;
    /// The def `(path, offset)` the use at `(path, offset)` resolves to.
    fn resolve_def_at(&self, path: &str, offset: u32) -> Option<(String, u32)>;
    /// The immediate base classes of the class at `(path, offset)`.
    fn supertypes_at(&self, path: &str, offset: u32) -> Vec<SuperClass>;
    /// Whether `member` is a top-level name of the module the binding resolves to.
    fn module_member(&self, path: &str, offset: u32, member: &str) -> MemberCheck;

    /// `(outgoing, incoming)` for each anchor. The recording's hot loop — ty
    /// overrides this to run the per-anchor queries (especially `incoming_calls`,
    /// which type-checks every call site) in parallel. Default is sequential.
    fn edges_batch(&self, anchors: &[(String, u32)]) -> Vec<(Vec<Neighbor>, Vec<Neighbor>)> {
        anchors.iter().map(|(p, o)| (self.outgoing_at(p, *o), self.incoming_at(p, *o))).collect()
    }

    /// `resolve_def_at` for each occurrence. Ty overrides this to parallelize.
    fn resolve_batch(&self, anchors: &[(String, u32)]) -> Vec<Option<(String, u32)>> {
        anchors.iter().map(|(p, o)| self.resolve_def_at(p, *o)).collect()
    }
}

impl CallGraphTy for TyResolver {
    fn outgoing_at(&self, path: &str, offset: u32) -> Vec<Neighbor> {
        TyResolver::outgoing_at(self, path, TextSize::from(offset))
    }
    fn incoming_at(&self, path: &str, offset: u32) -> Vec<Neighbor> {
        TyResolver::incoming_at(self, path, TextSize::from(offset))
    }
    fn resolve_def_at(&self, path: &str, offset: u32) -> Option<(String, u32)> {
        TyResolver::resolve_def_at(self, path, TextSize::from(offset))
    }
    fn supertypes_at(&self, path: &str, offset: u32) -> Vec<SuperClass> {
        TyResolver::supertypes_at(self, path, offset)
    }
    fn module_member(&self, path: &str, offset: u32, member: &str) -> MemberCheck {
        TyResolver::module_member(self, path, offset, member)
    }

    // The per-anchor ty queries (especially `incoming_calls`, which type-checks
    // every call site) are the cold index's dominant cost and are independent, so
    // they run in parallel. ty's `ProjectDatabase` isn't `Sync` (per-thread salsa
    // state), but it *is* cheaply `Clone` — so each task gets its own handle over
    // the same shared storage. Cloning happens on this thread, before the move
    // into the task, exactly as ty's own checker fans out.
    fn edges_batch(&self, anchors: &[(String, u32)]) -> Vec<(Vec<Neighbor>, Vec<Neighbor>)> {
        use rayon::prelude::*;
        anchors
            .iter()
            .map(|a| (self.clone(), a))
            .collect::<Vec<_>>()
            .into_par_iter()
            .map(|(ty, (p, o))| {
                (ty.outgoing_at(p, TextSize::from(*o)), ty.incoming_at(p, TextSize::from(*o)))
            })
            .collect()
    }

    fn resolve_batch(&self, anchors: &[(String, u32)]) -> Vec<Option<(String, u32)>> {
        use rayon::prelude::*;
        anchors
            .iter()
            .map(|a| (self.clone(), a))
            .collect::<Vec<_>>()
            .into_par_iter()
            .map(|(ty, (p, o))| ty.resolve_def_at(p, TextSize::from(*o)))
            .collect()
    }
}

/// A serializable [`Neighbor`] — the cache can't store `Neighbor`'s
/// `&'static str` kind, so it round-trips through an owned `String` and
/// re-interns on the way out.
#[derive(Clone, Serialize, Deserialize)]
struct NeighborData {
    path: String,
    offset: u32,
    line: usize,
    col: usize,
    name: String,
    kind: String,
}

fn neighbor_data(n: &Neighbor) -> NeighborData {
    NeighborData {
        path: n.path.clone(),
        offset: n.offset,
        line: n.line,
        col: n.col,
        name: n.name.clone(),
        kind: n.kind.to_string(),
    }
}

fn to_data(nbs: &[Neighbor]) -> Vec<NeighborData> {
    nbs.iter().map(neighbor_data).collect()
}

/// How a def anchor renders when it appears as a *caller* in the transposed
/// `incoming` map: the same `(path, offset, line, col, name, kind)` ty would
/// report. `kind` mirrors ty's symbol kind — a function whose immediate scope is
/// a class is a `method`, otherwise a `function`.
fn def_neighbor(
    f: &FileIndex,
    d: &pyq_index::Def,
    class_scopes: &HashSet<Vec<String>>,
) -> NeighborData {
    let kind = match d.kind {
        DefKind::Class => "class",
        _ if class_scopes.contains(&d.container) => "method",
        _ => "function",
    };
    NeighborData {
        path: f.path.clone(),
        offset: d.offset,
        line: d.pos.line,
        col: d.pos.col,
        name: d.name.clone(),
        kind: kind.to_string(),
    }
}

/// The synthetic caller node for module-scope (import-time) code in `path` — the
/// module itself, anchored at its start. Matches how ty surfaces a module-scope
/// caller: `path:1:1`, kind `module`, FQN the module path (see [`fallback_fqn`],
/// which ignores the name for a module).
fn module_neighbor(path: &str) -> NeighborData {
    NeighborData {
        path: path.to_string(),
        offset: 0,
        line: 1,
        col: 1,
        name: module_components(path).into_iter().last().unwrap_or_default().to_string(),
        kind: "module".to_string(),
    }
}

fn from_data(data: &[NeighborData]) -> Vec<Neighbor> {
    data.iter()
        .map(|d| Neighbor {
            path: d.path.clone(),
            offset: d.offset,
            line: d.line,
            col: d.col,
            name: d.name.clone(),
            kind: intern_kind(&d.kind),
        })
        .collect()
}

/// Re-intern a kind string to the `&'static str` the rest of the code expects.
/// The set is closed (it comes from ty's `SymbolKind` mapping); anything else
/// collapses to `"callable"`, the same default that mapping uses.
fn intern_kind(kind: &str) -> &'static str {
    match kind {
        "class" => "class",
        "method" => "method",
        "function" => "function",
        "property" => "property",
        "module" => "module",
        _ => "callable",
    }
}

/// Every ty answer the [`CallGraph`] traversals need, recorded once so they can
/// be replayed without ty. Keyed by `(path, offset)`; bincode handles the
/// non-string keys (the cache never serializes this to JSON).
#[derive(Default, Clone, Serialize, Deserialize)]
pub struct GraphRecording {
    outgoing: HashMap<(String, u32), Vec<NeighborData>>,
    incoming: HashMap<(String, u32), Vec<NeighborData>>,
    resolve: HashMap<(String, u32), Option<(String, u32)>>,
    supertypes: HashMap<(String, u32), Vec<SuperClass>>,
}

/// A [`CallGraphTy`] that answers from a [`GraphRecording`] — no ty. A query
/// for an unrecorded `(path, offset)` returns the empty/`None` answer, which is
/// what live ty returns for an offset with no edges, so a complete recording
/// (see [`CallGraph::record`]) replays identically. `module_member` is never
/// recorded (only `mock-targets` uses it, and it stays on live ty), so it
/// reports `Unknown`.
pub struct ReplayTy {
    rec: GraphRecording,
}

impl CallGraphTy for ReplayTy {
    fn outgoing_at(&self, path: &str, offset: u32) -> Vec<Neighbor> {
        self.rec
            .outgoing
            .get(&(path.to_string(), offset))
            .map(|d| from_data(d))
            .unwrap_or_default()
    }
    fn incoming_at(&self, path: &str, offset: u32) -> Vec<Neighbor> {
        self.rec
            .incoming
            .get(&(path.to_string(), offset))
            .map(|d| from_data(d))
            .unwrap_or_default()
    }
    fn resolve_def_at(&self, path: &str, offset: u32) -> Option<(String, u32)> {
        self.rec
            .resolve
            .get(&(path.to_string(), offset))
            .cloned()
            .flatten()
    }
    fn supertypes_at(&self, path: &str, offset: u32) -> Vec<SuperClass> {
        self.rec
            .supertypes
            .get(&(path.to_string(), offset))
            .cloned()
            .unwrap_or_default()
    }
    fn module_member(&self, _path: &str, _offset: u32, _member: &str) -> MemberCheck {
        MemberCheck::Unknown
    }
}
