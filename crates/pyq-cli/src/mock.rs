//! Resolve `mock.patch("a.b.c")` target strings against the project's symbol
//! structure to flag drifted patch paths.
//!
//! The bug this catches: `patch` replaces a name *where it is looked up*, not
//! where it is defined — so a test patches `myapp.client.requests` because
//! `client.py` does `import requests`. Refactor that import away and the patch
//! silently no-ops; the test still passes, now exercising the real `requests`.
//! Because the syntactic index records import bindings as defs, we can resolve
//! the dotted target against each module's actual top-level names and report
//! the ones that no longer resolve.
//!
//! Precision over recall: a target is only called **drifted** when its module
//! prefix is a project module *and* the looked-up name is provably absent from
//! it. Targets into third-party/stdlib modules (not indexed) and computed
//! (non-literal) targets are reported as unchecked, never as broken.

use std::collections::{HashMap, HashSet};

use pyq_index::{DefKind, FileIndex};
use pyq_resolve::scope_fqn;

/// The verdict for one patch target.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Status {
    /// Resolves to a real project symbol.
    Valid,
    /// Module prefix is a project module, but the name isn't bound there — the
    /// patch silently does nothing. The string explains what's missing.
    Drifted(String),
    /// No project-module prefix — patches a third-party/stdlib lookup path we
    /// don't index. Not checked.
    External,
    /// The first argument wasn't a string literal (computed target).
    Dynamic,
    /// Resolved partway, but the rest is an attribute on a non-class binding
    /// (e.g. a method on an imported object) we can't introspect statically.
    Unverifiable(String),
}

impl Status {
    /// Short tag for output.
    pub fn tag(&self) -> &'static str {
        match self {
            Status::Valid => "valid",
            Status::Drifted(_) => "drifted",
            Status::External => "external",
            Status::Dynamic => "dynamic",
            Status::Unverifiable(_) => "unverifiable",
        }
    }
}

/// Common builtins reachable as a module attribute. `patch("mod.input")`
/// resolves through the module namespace even though `input` is never defined or
/// imported there, so these must not read as drifted. (Patching a builtin is a
/// standard idiom — `input`, `open`, `print`.)
const BUILTINS: &[&str] = &[
    "input", "open", "print", "len", "range", "exit", "quit", "vars", "eval",
    "exec", "compile", "id", "hash", "repr", "format", "isinstance", "getattr",
    "setattr", "hasattr", "delattr", "next", "iter", "super", "type", "object",
];

/// The project's module + symbol structure, indexed for patch-target resolution.
pub struct PatchResolver {
    modules: HashSet<String>,
    /// module id → its top-level bound names (functions, classes, vars, imports).
    module_names: HashMap<String, HashSet<String>>,
    /// module id → classes defined at module scope.
    module_classes: HashMap<String, HashSet<String>>,
    /// (module id, class name) → that class's direct member names.
    class_members: HashMap<(String, String), HashSet<String>>,
    /// (module id, class name) of classes that extend a base (beyond `object`).
    /// A missing attribute on such a class may be inherited or framework-injected
    /// (Django's `objects` manager, `Model._save_table`), so it isn't drift.
    subclasses: HashSet<(String, String)>,
}

impl PatchResolver {
    pub fn build(files: &[FileIndex]) -> Self {
        let mut modules = HashSet::new();
        let mut module_names: HashMap<String, HashSet<String>> = HashMap::new();
        let mut module_classes: HashMap<String, HashSet<String>> = HashMap::new();
        let mut class_members: HashMap<(String, String), HashSet<String>> = HashMap::new();
        let mut subclasses = HashSet::new();

        for f in files {
            let module = scope_fqn(&f.path, &[]);
            modules.insert(module.clone());
            let names = module_names.entry(module.clone()).or_default();
            for d in &f.defs {
                match d.container.as_slice() {
                    // Module scope: a name you can patch as `module.<name>`.
                    [] => {
                        names.insert(d.name.clone());
                        if d.kind == DefKind::Class {
                            module_classes
                                .entry(module.clone())
                                .or_default()
                                .insert(d.name.clone());
                            // A base other than `object` means inherited /
                            // framework-injected members are possible.
                            if d.bases.iter().any(|b| b != "object") {
                                subclasses.insert((module.clone(), d.name.clone()));
                            }
                        }
                    }
                    // Direct member of a top-level container (class method/attr,
                    // patched as `module.Class.<name>`).
                    [container] => {
                        class_members
                            .entry((module.clone(), container.clone()))
                            .or_default()
                            .insert(d.name.clone());
                    }
                    _ => {}
                }
            }
        }
        PatchResolver {
            modules,
            module_names,
            module_classes,
            class_members,
            subclasses,
        }
    }

    /// Resolve a patch target (`None` = the argument wasn't a literal).
    pub fn resolve(&self, target: Option<&str>) -> Status {
        let Some(t) = target else {
            return Status::Dynamic;
        };
        let parts: Vec<&str> = t.split('.').filter(|s| !s.is_empty()).collect();
        if parts.is_empty() {
            return Status::Dynamic;
        }
        // Longest dotted prefix that names a project module — the rest is the
        // attribute chain looked up on it at patch time.
        for k in (1..=parts.len()).rev() {
            let prefix = parts[..k].join(".");
            if let Some(module) = self.canonical_module(&prefix) {
                return self.resolve_attr(&module, &parts[k..]);
            }
        }
        Status::External
    }

    /// Map a module spelling as written in a patch string to its canonical
    /// file-derived id, honoring a source root: an exact match, else the *unique*
    /// project module whose id ends with `.<spelling>` (the nested-root case,
    /// where code is rooted at `alice/alice/` so patches read `main.services.x`
    /// while the file id is `alice.alice.main.services.x`). Ambiguous or
    /// unmatched → `None`. Mirrors the import graph's `canonicalize_target`.
    fn canonical_module(&self, spelling: &str) -> Option<String> {
        if self.modules.contains(spelling) {
            return Some(spelling.to_string());
        }
        let suffix = format!(".{spelling}");
        let mut matches = self.modules.iter().filter(|m| m.ends_with(&suffix));
        match (matches.next(), matches.next()) {
            (Some(only), None) => Some(only.clone()),
            _ => None,
        }
    }

    fn resolve_attr(&self, module: &str, chain: &[&str]) -> Status {
        // Patching the module object itself.
        let Some(first) = chain.first() else {
            return Status::Valid;
        };
        let bound = self
            .module_names
            .get(module)
            .is_some_and(|n| n.contains(*first));
        if !bound {
            // A builtin reachable through the module namespace (`patch("m.input")`)
            // is valid even though it's neither defined nor imported there.
            if BUILTINS.contains(first) {
                return Status::Valid;
            }
            return Status::Drifted(format!("`{first}` is not bound in module `{module}`"));
        }
        if chain.len() == 1 {
            return Status::Valid;
        }
        // Deeper than `module.name`: only verifiable when `name` is a class we
        // indexed — then check the next element is one of its members.
        let is_class = self
            .module_classes
            .get(module)
            .is_some_and(|c| c.contains(*first));
        if !is_class {
            return Status::Unverifiable(format!(
                "`{module}.{first}` is not a project class; can't check `.{}`",
                chain[1..].join(".")
            ));
        }
        let member = chain[1];
        let known = self
            .class_members
            .get(&(module.to_string(), first.to_string()))
            .is_some_and(|m| m.contains(member));
        if !known {
            // If the class extends a base we can't see into, the member may be
            // inherited or framework-injected (Django `objects`, `_save_table`),
            // so it isn't provably absent — don't flag it as drift.
            if self.subclasses.contains(&(module.to_string(), first.to_string())) {
                return Status::Unverifiable(format!(
                    "`{member}` not declared on `{module}.{first}`, but it extends a base — may be inherited"
                ));
            }
            return Status::Drifted(format!(
                "`{member}` is not a member of `{module}.{first}`"
            ));
        }
        if chain.len() == 2 {
            Status::Valid
        } else {
            Status::Unverifiable(format!(
                "nested attribute `.{}` beyond class member",
                chain[2..].join(".")
            ))
        }
    }
}
