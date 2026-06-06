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

/// The project's module + symbol structure, indexed for patch-target resolution.
pub struct PatchResolver {
    modules: HashSet<String>,
    /// module id → its top-level bound names (functions, classes, vars, imports).
    module_names: HashMap<String, HashSet<String>>,
    /// module id → classes defined at module scope.
    module_classes: HashMap<String, HashSet<String>>,
    /// (module id, class name) → that class's direct member names.
    class_members: HashMap<(String, String), HashSet<String>>,
}

impl PatchResolver {
    pub fn build(files: &[FileIndex]) -> Self {
        let mut modules = HashSet::new();
        let mut module_names: HashMap<String, HashSet<String>> = HashMap::new();
        let mut module_classes: HashMap<String, HashSet<String>> = HashMap::new();
        let mut class_members: HashMap<(String, String), HashSet<String>> = HashMap::new();

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
            let module = parts[..k].join(".");
            if self.modules.contains(&module) {
                return self.resolve_attr(&module, &parts[k..]);
            }
        }
        Status::External
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
