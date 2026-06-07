//! Class inheritance graph + override map (#12 `hierarchy`).
//!
//! The OO-refactor footgun: change a base method and you must find every
//! override; add an abstract method and you must find every subclass that has to
//! implement it. pyq answers structurally. ty resolves each class's *immediate*
//! bases (`supertypes_at`, through imports and across files); we index that for
//! the whole project and invert it to get subclasses, walk it for supers, and
//! cross it with the per-class method sets to get the override map.
//!
//! A base ty can't resolve to a first-party class (a third-party/stdlib base, or
//! one whose import isn't installed) is recorded as *external* — the signal that
//! a class is framework-managed, which `deadcode` reads to keep such classes and
//! their methods (and their inner config classes) live.

use std::collections::{HashMap, HashSet};

use pyq_index::{DefKind, FileIndex};
use pyq_resolve::{scope_fqn, CallGraph};

/// The project's class inheritance graph.
pub struct Hierarchy {
    /// class FQN → immediate first-party base FQNs.
    bases: HashMap<String, Vec<String>>,
    /// base FQN → immediate first-party subclass FQNs (the inverted graph).
    children: HashMap<String, Vec<String>>,
    /// class FQN → external/unresolved base display names (`models.Model`).
    external_bases: HashMap<String, Vec<String>>,
    /// Classes with at least one external/unresolved base — framework-managed.
    has_external_base: HashSet<String>,
    /// class FQN → its directly-defined method names.
    methods: HashMap<String, HashSet<String>>,
    /// class FQN → its `(path, name offset)`.
    anchor: HashMap<String, (String, u32)>,
}

impl Hierarchy {
    pub fn build(files: &[FileIndex], graph: &CallGraph) -> Self {
        // First pass: index classes (anchors, syntactic bases) and methods.
        let mut anchor: HashMap<String, (String, u32)> = HashMap::new();
        let mut anchor_to_fqn: HashMap<(String, u32), String> = HashMap::new();
        let mut syntactic_bases: HashMap<String, Vec<String>> = HashMap::new();
        let mut methods: HashMap<String, HashSet<String>> = HashMap::new();
        for f in files {
            for d in &f.defs {
                match d.kind {
                    DefKind::Class => {
                        let mut scope = d.container.clone();
                        scope.push(d.name.clone());
                        let fqn = scope_fqn(&f.path, &scope);
                        anchor.insert(fqn.clone(), (f.path.clone(), d.offset));
                        anchor_to_fqn.insert((f.path.clone(), d.offset), fqn.clone());
                        syntactic_bases.insert(fqn, d.bases.clone());
                    }
                    // A method: its enclosing scope is its owning class's FQN.
                    DefKind::Function if !d.container.is_empty() => {
                        methods
                            .entry(scope_fqn(&f.path, &d.container))
                            .or_default()
                            .insert(d.name.clone());
                    }
                    _ => {}
                }
            }
        }

        // Second pass: resolve bases via ty for every class that writes one.
        let mut bases: HashMap<String, Vec<String>> = HashMap::new();
        let mut children: HashMap<String, Vec<String>> = HashMap::new();
        let mut external_bases: HashMap<String, Vec<String>> = HashMap::new();
        let mut has_external_base: HashSet<String> = HashSet::new();
        for (fqn, (path, offset)) in &anchor {
            let written: Vec<&String> = syntactic_bases[fqn]
                .iter()
                .filter(|b| *b != "object")
                .collect();
            if written.is_empty() {
                continue; // no explicit base → nothing to resolve
            }
            let mut project_count = 0;
            for sup in graph.supertypes_at(path, *offset) {
                match sup.anchor.and_then(|a| anchor_to_fqn.get(&a)) {
                    Some(base_fqn) => {
                        project_count += 1;
                        bases.entry(fqn.clone()).or_default().push(base_fqn.clone());
                        children.entry(base_fqn.clone()).or_default().push(fqn.clone());
                    }
                    None => external_bases
                        .entry(fqn.clone())
                        .or_default()
                        .push(sup.name),
                }
            }
            // A written base that didn't resolve to a first-party class is
            // external (third-party, stdlib, or an uninstalled import) — robust
            // even when ty can't see the dependency.
            if project_count < written.len() {
                has_external_base.insert(fqn.clone());
            }
        }

        Hierarchy {
            bases,
            children,
            external_bases,
            has_external_base,
            methods,
            anchor,
        }
    }

    /// Whether this class extends a base pyq couldn't resolve to a first-party
    /// class — the "framework-managed" signal.
    pub fn has_external_base(&self, fqn: &str) -> bool {
        self.has_external_base.contains(fqn)
    }

    /// Whether this class *or any transitive first-party ancestor* extends an
    /// external base — so a Django model `Foo(TimeStampedModel)` is framework-
    /// managed even though its immediate base is first-party, because the chain
    /// reaches `models.Model`. This is the signal `deadcode`/`mock-targets` want:
    /// the framework drives the whole inheritance chain, not just direct subclasses.
    pub fn has_external_ancestor(&self, fqn: &str) -> bool {
        self.has_external_base(fqn)
            || self.supers(fqn).iter().any(|s| self.has_external_base(s))
    }

    /// External base display names of a class (`["models.Model"]`).
    pub fn external_bases(&self, fqn: &str) -> &[String] {
        self.external_bases.get(fqn).map_or(&[], Vec::as_slice)
    }

    /// Every class FQN in the project (insertion order not guaranteed).
    pub fn class_fqns(&self) -> impl Iterator<Item = &String> {
        self.anchor.keys()
    }

    /// The transitive first-party base classes of `fqn`, nearest first.
    pub fn supers(&self, fqn: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        let mut queue: std::collections::VecDeque<String> =
            self.bases.get(fqn).into_iter().flatten().cloned().collect();
        while let Some(b) = queue.pop_front() {
            if !seen.insert(b.clone()) {
                continue;
            }
            out.push(b.clone());
            queue.extend(self.bases.get(&b).into_iter().flatten().cloned());
        }
        out
    }

    /// The transitive first-party subclasses of `fqn`.
    pub fn subclasses(&self, fqn: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        let mut queue: std::collections::VecDeque<String> =
            self.children.get(fqn).into_iter().flatten().cloned().collect();
        while let Some(c) = queue.pop_front() {
            if !seen.insert(c.clone()) {
                continue;
            }
            out.push(c.clone());
            queue.extend(self.children.get(&c).into_iter().flatten().cloned());
        }
        out
    }

    /// The method names directly defined on a class.
    pub fn methods(&self, fqn: &str) -> Option<&HashSet<String>> {
        self.methods.get(fqn)
    }

    /// First-party base classes that define a method named `method` — the ones a
    /// `class_fqn.method` definition overrides. Nearest-first.
    pub fn overrides(&self, class_fqn: &str, method: &str) -> Vec<String> {
        self.supers(class_fqn)
            .into_iter()
            .filter(|b| self.methods.get(b).is_some_and(|m| m.contains(method)))
            .collect()
    }

    /// First-party subclasses that override `class_fqn.method` (define the same
    /// method name) — paired with their FQN for display.
    pub fn overridden_by(&self, class_fqn: &str, method: &str) -> Vec<String> {
        self.subclasses(class_fqn)
            .into_iter()
            .filter(|s| self.methods.get(s).is_some_and(|m| m.contains(method)))
            .collect()
    }
}
