//! The `deadcode` verb — functions and classes reachable from no entrypoint.
//!
//! A projection of the [`CallGraph`] (#10) run *forward from the roots*: compute
//! everything reachable from the program's entrypoints, and report the callables
//! that aren't in it. The whole difficulty is the root set — Python has no single
//! `main`, and most "live" code is reached by *convention or config*, not by a
//! call from project code. Flagging such code dead is the dangerous failure
//! (someone deletes a live route handler), so the bias is heavily toward calling
//! things live; this over-reports liveness and under-reports death, and the
//! residual is flagged rather than hidden.
//!
//! Roots (anything the runtime/framework may enter without a project call):
//!   - pytest-collected tests, and *all* methods of a collected test class
//!     (`setUp`/fixtures are framework-invoked, not just `test_*`);
//!   - dunder methods (`__init__`, `__enter__`, … — runtime-invoked);
//!   - decorated callables (routes, fixtures, tasks, CLI commands, signals);
//!   - `__all__` exports (the declared public surface);
//!   - callables referenced at module scope (`__main__` blocks, URL tables,
//!     registries) — resolved through ty;
//!   - everything in an entrypoint *file* (`manage.py`, `wsgi.py`/`asgi.py`,
//!     `urls.py`, `settings`, `conftest.py`, `migrations/`, `management/commands/`,
//!     `scripts/`, `setup.py`) and every method of a framework base subclass
//!     (`BaseCommand`, `AppConfig`, `Migration`, `*View`/`*ViewSet`, …);
//!   - `[project.scripts]` / `[tool.poetry.scripts]` console entrypoints.
//!
//! Reachability rides ty's call hierarchy, which *does* follow attribute calls in
//! a body (`self.repo.save()`), so forward reachability is fairly complete. The
//! residual false-positive source is genuine dynamic dispatch — a callable
//! reached only via reflection / a registry / `getattr` — which is flagged.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use pyq_index::{Def, DefKind, FileIndex};
use pyq_resolve::{scope_fqn, CallGraph};

use crate::hierarchy::Hierarchy;
use crate::tests_map::{is_collected_test_def, test_class_fqns};

/// One callable with no path from any entrypoint.
pub struct Dead {
    pub path: String,
    pub line: usize,
    pub col: usize,
    pub fqn: String,
    pub kind: &'static str,
}

/// The dead-code result: candidates plus the population they were drawn from.
pub struct DeadCode {
    pub dead: Vec<Dead>,
    pub total: usize,
}

/// Find callables reachable from no entrypoint. `graph` and `files` must be built
/// over the same tree; `root` is the project root (for reading `pyproject.toml`).
pub fn find(files: &[FileIndex], graph: &CallGraph, root: &str) -> DeadCode {
    let test_classes = test_class_fqns(files);
    let console = console_script_targets(root);
    let hier = Hierarchy::build(files, graph);

    // Classes whose every method is framework-invoked (so a method's presence in
    // the call graph isn't required for it to be live).
    let entry_classes = framework_managed_classes(files, &hier, &test_classes);

    // Names defined anywhere in the project — a cheap filter so we only pay ty to
    // resolve a module-scope reference that *could* name a first-party callable.
    let def_names: HashSet<&str> = files
        .iter()
        .flat_map(|f| f.defs.iter().map(|d| d.name.as_str()))
        .collect();

    // FQN → anchor for every callable, so a dotted-string config path
    // (`'api.utils.handler'`) can be resolved to a real def and seeded.
    let def_anchor = def_anchors(files);

    // Collect root anchors: (path, name offset) of every callable that is live by
    // entry. Deduped because a def can satisfy several rules.
    let mut seeds: HashSet<(String, u32)> = HashSet::new();
    for f in files {
        let entry_file = is_entrypoint_file(&f.path);
        for d in &f.defs {
            if !matches!(d.kind, DefKind::Function | DefKind::Class) {
                continue;
            }
            // Live if it's framework/convention entry glue, a collected test, or
            // a declared `__all__` export (externally importable).
            let is_root = is_framework_entry(d, &f.path, entry_file, &entry_classes, &console)
                || is_collected_test_def(d, &f.path, &test_classes)
                || (d.container.is_empty() && f.dunder_all.contains(&d.name));
            if is_root {
                seeds.insert((f.path.clone(), d.offset));
            }
        }
        // Module-scope references keep their target live (`main()` under
        // `__main__`, a view in a URL table). Resolve only those that could name
        // a first-party callable.
        for r in &f.refs {
            if r.module_scope && def_names.contains(r.name.as_str()) {
                if let Some(anchor) = graph.resolve_anchor(&f.path, r.offset) {
                    seeds.insert(anchor);
                }
            }
        }
        // A dotted-string config path naming a project callable keeps it live —
        // the framework invokes it by that string (Django `EXCEPTION_HANDLER`,
        // Celery task names, entry points), invisible to the call graph.
        for s in &f.dotted_strings {
            if let Some(anchor) = resolve_dotted(&def_anchor, s) {
                seeds.insert(anchor);
            }
        }
    }

    // Override edges: a reachable base method makes its overrides reachable — the
    // polymorphic call the graph misses (a base-typed `x.method()` resolves to
    // the base, not each concrete override). The BFS folds them in as extra
    // successors.
    let override_edges = override_edges(&hier, &def_anchor);

    let seed_vec: Vec<(String, u32)> = seeds.into_iter().collect();
    let reachable = graph.reachable_from(&seed_vec, &override_edges);

    // A callable is dead if its FQN isn't reachable. Suppress a method whose
    // enclosing class is itself dead — the class subsumes it (less noise).
    let mut dead_fqns: HashSet<String> = HashSet::new();
    let mut total = 0usize;
    let mut candidates: Vec<Dead> = Vec::new();
    for f in files {
        for d in &f.defs {
            if !matches!(d.kind, DefKind::Function | DefKind::Class) {
                continue;
            }
            total += 1;
            let fqn = scope_fqn(&f.path, &def_scope(d));
            if reachable.contains(&fqn) {
                continue;
            }
            dead_fqns.insert(fqn.clone());
            candidates.push(Dead {
                path: f.path.clone(),
                line: d.pos.line,
                col: d.pos.col,
                fqn,
                kind: if d.kind == DefKind::Class { "class" } else if d.container.is_empty() { "function" } else { "method" },
            });
        }
    }
    let mut dead: Vec<Dead> = candidates
        .into_iter()
        .filter(|c| {
            // Drop a dead method when its enclosing class is also dead.
            !c.fqn.rsplit_once('.').is_some_and(|(parent, _)| {
                c.kind == "method" && dead_fqns.contains(parent)
            })
        })
        .collect();
    dead.sort_by(|a, b| (&a.path, a.line, a.col).cmp(&(&b.path, b.line, b.col)));
    DeadCode { dead, total }
}

/// Resolve a dotted-string config path to the callable it names, if any —
/// matching against project def FQNs exactly, else by *unique* suffix (the
/// source-root case: config says `main.x.handler`, the file id is
/// `alice.main.x.handler`). The `module:attr` entry-point form is normalized to
/// a dot. `None` when it names nothing first-party (so a coincidental
/// path-shaped string is ignored, never seeded).
fn resolve_dotted(
    def_anchor: &HashMap<String, (String, u32)>,
    s: &str,
) -> Option<(String, u32)> {
    let fqn = s.replacen(':', ".", 1);
    if let Some(anchor) = def_anchor.get(&fqn) {
        return Some(anchor.clone());
    }
    let suffix = format!(".{fqn}");
    let mut matches = def_anchor.iter().filter(|(k, _)| k.ends_with(&suffix));
    match (matches.next(), matches.next()) {
        (Some((_, anchor)), None) => Some(anchor.clone()),
        _ => None,
    }
}

/// A def's full scope path (enclosing scopes + its own name) — the input to
/// [`scope_fqn`] for its FQN.
fn def_scope(d: &Def) -> Vec<String> {
    let mut s = d.container.clone();
    s.push(d.name.clone());
    s
}

/// A class def's scope path — same as [`def_scope`] (kept for intent at call site).
fn class_scope(d: &Def) -> Vec<String> {
    def_scope(d)
}

fn is_dunder(name: &str) -> bool {
    name.starts_with("__") && name.ends_with("__") && name.len() > 4
}

/// Classes whose every method is framework-invoked — so a method's presence in
/// the call graph isn't required for it to be live: a class in an entrypoint
/// file, a collected test class, or — the general signal — one that extends a
/// base ty can't resolve to a first-party class (Django/DRF/stdlib/uninstalled).
/// The last subsumes a curated suffix list AND a project's own framework usage,
/// and is what handles the polymorphic-override case (a `BasePermission`
/// subclass's `has_permission` is live by inheritance).
fn framework_managed_classes(
    files: &[FileIndex],
    hier: &Hierarchy,
    test_classes: &HashSet<String>,
) -> HashSet<String> {
    let mut out: HashSet<String> = test_classes.clone();
    for f in files {
        for d in &f.defs {
            if d.kind == DefKind::Class {
                let fqn = scope_fqn(&f.path, &class_scope(d));
                if is_entrypoint_file(&f.path) || hier.has_external_ancestor(&fqn) {
                    out.insert(fqn);
                }
            }
        }
    }
    out
}

/// Whether a def is framework/convention entry glue the runtime enters without a
/// project call: decorated (route/fixture/task/signal/CLI), a dunder, anything
/// in an entrypoint file, part of a framework-managed class's subtree (the
/// class, its methods, its inner `Meta`/`Config`), or a console-script target.
/// The shared core of `deadcode`'s root rule and `canonical`'s framework-aware
/// untested-public filter. `__all__` membership and test-collection are
/// deliberately *not* here — they're per-verb liveness reasons, and an
/// exported-but-untested symbol is a finding `canonical` keeps, not hides.
fn is_framework_entry(
    d: &Def,
    path: &str,
    entry_file: bool,
    entry_classes: &HashSet<String>,
    console: &[String],
) -> bool {
    let fqn = scope_fqn(path, &def_scope(d));
    let in_entry_class = (1..=d.container.len())
        .any(|k| entry_classes.contains(&scope_fqn(path, &d.container[..k])));
    let is_entry_class = d.kind == DefKind::Class && entry_classes.contains(&fqn);
    d.decorated
        || is_dunder(&d.name)
        || entry_file
        || in_entry_class
        || is_entry_class
        || console.iter().any(|t| matches_console(t, &fqn))
}

/// The FQNs that are framework-driven entrypoints — what the framework/runtime
/// enters without a direct project call: [`is_framework_entry`] defs, plus the
/// targets of module-scope references and dotted-string config (a handler named
/// by string in settings). `canonical` subtracts this set from its public
/// surface so the untested-public list isn't swamped by serializers, configs,
/// migrations, commands, routers and tasks the framework drives (they're
/// exercised through dispatch, not a direct test call). `__all__` exports are
/// **not** subtracted — a declared-public symbol no test reaches is exactly the
/// finding to keep. Mirrors `find`'s root rule minus the `__all__`/test seeds.
pub fn framework_entry_fqns(
    files: &[FileIndex],
    graph: &CallGraph,
    hier: &Hierarchy,
    root: &str,
) -> HashSet<String> {
    let test_classes = test_class_fqns(files);
    let console = console_script_targets(root);
    let entry_classes = framework_managed_classes(files, hier, &test_classes);
    let def_anchor = def_anchors(files);
    // Reverse of `def_anchor`, to name the target of a resolved reference/string.
    let fqn_by_anchor: HashMap<(String, u32), String> = def_anchor
        .iter()
        .map(|(fqn, anchor)| (anchor.clone(), fqn.clone()))
        .collect();
    let def_names: HashSet<&str> = files
        .iter()
        .flat_map(|f| f.defs.iter().map(|d| d.name.as_str()))
        .collect();

    let mut out: HashSet<String> = HashSet::new();
    for f in files {
        let entry_file = is_entrypoint_file(&f.path);
        for d in &f.defs {
            if matches!(d.kind, DefKind::Function | DefKind::Class)
                && is_framework_entry(d, &f.path, entry_file, &entry_classes, &console)
            {
                out.insert(scope_fqn(&f.path, &def_scope(d)));
            }
        }
        // A module-scope reference drives its target (a view in a URL table, a
        // handler in a registry).
        for r in &f.refs {
            if r.module_scope && def_names.contains(r.name.as_str()) {
                if let Some(anchor) = graph.resolve_anchor(&f.path, r.offset) {
                    if let Some(fqn) = fqn_by_anchor.get(&anchor) {
                        out.insert(fqn.clone());
                    }
                }
            }
        }
        // A dotted-string config path names a callable the framework invokes.
        for s in &f.dotted_strings {
            if let Some(anchor) = resolve_dotted(&def_anchor, s) {
                if let Some(fqn) = fqn_by_anchor.get(&anchor) {
                    out.insert(fqn.clone());
                }
            }
        }
    }
    out
}

/// Why a reverse-reachability answer for a symbol is framework-driven — the
/// reason the call graph can't see all of its callers.
pub enum DispatchKind {
    /// Decorated — a route/task/fixture/signal/CLI hook the framework calls.
    Decorated,
    /// A method on a class that extends a non-project base (Django/DRF/stdlib),
    /// so the framework drives it polymorphically.
    FrameworkBase,
    /// Entered by convention/config: an entrypoint file, a module-scope
    /// registry reference, or a dotted-string config path.
    FrameworkEntry,
}

impl DispatchKind {
    /// A short clause naming the reason, for a one-line note.
    pub fn reason(&self) -> &'static str {
        match self {
            DispatchKind::Decorated => "decorated, so the framework calls it",
            DispatchKind::FrameworkBase => {
                "a method on a framework-driven class (extends a non-project base)"
            }
            DispatchKind::FrameworkEntry => {
                "entered by convention/config (entrypoint file, registry, or a dotted-string path)"
            }
        }
    }
}

/// Evidence that reverse reachability for `roots` is **incomplete** because the
/// symbol is framework-driven — entered without a direct project call, so
/// callers/tests that reach it only through that dispatch aren't in the call
/// graph. Returns `None` when there's no such evidence, so consumers
/// (`tests`/`callers`/`graph --reverse`) fire the caveat *only when it applies*
/// rather than as a blanket disclaimer — turning a misleading `0` into a
/// specific, trustworthy note. Reuses the same liveness signals as `deadcode`.
pub fn dispatch_caveat(
    roots: &[String],
    files: &[FileIndex],
    graph: &CallGraph,
    hier: &Hierarchy,
    root: &str,
) -> Option<(String, DispatchKind)> {
    if roots.is_empty() {
        return None;
    }
    let root_set: HashSet<&str> = roots.iter().map(String::as_str).collect();
    let test_classes = test_class_fqns(files);
    let entry_classes = framework_managed_classes(files, hier, &test_classes);

    // Classify the queried def(s) directly — gives a specific reason.
    for f in files {
        let entry_file = is_entrypoint_file(&f.path);
        for d in &f.defs {
            let fqn = scope_fqn(&f.path, &def_scope(d));
            if !root_set.contains(fqn.as_str()) {
                continue;
            }
            if d.decorated {
                return Some((fqn, DispatchKind::Decorated));
            }
            let in_entry_class = (1..=d.container.len())
                .any(|k| entry_classes.contains(&scope_fqn(&f.path, &d.container[..k])));
            let is_entry_class = d.kind == DefKind::Class && entry_classes.contains(&fqn);
            if in_entry_class || is_entry_class {
                return Some((fqn, DispatchKind::FrameworkBase));
            }
            if entry_file {
                return Some((fqn, DispatchKind::FrameworkEntry));
            }
        }
    }
    // Fallback: a module-scope reference or dotted-string config names a root
    // (a view in a URL table, a handler in settings) — framework-driven, even
    // though the def itself carries no local marker.
    let entry = framework_entry_fqns(files, graph, hier, root);
    roots
        .iter()
        .find(|f| entry.contains(*f))
        .map(|f| (f.clone(), DispatchKind::FrameworkEntry))
}

/// FQN → `(path, name offset)` for every first-party callable (function/class)
/// in the tree — the anchor a graph walk seeds from, and the lookup a
/// dotted-string config path or override edge resolves against. Shared by
/// `deadcode` and `canonical`, which both reach from a seed set over the same
/// callables.
pub fn def_anchors(files: &[FileIndex]) -> HashMap<String, (String, u32)> {
    let mut anchors: HashMap<String, (String, u32)> = HashMap::new();
    for f in files {
        for d in &f.defs {
            if matches!(d.kind, DefKind::Function | DefKind::Class) {
                anchors
                    .entry(scope_fqn(&f.path, &def_scope(d)))
                    .or_insert((f.path.clone(), d.offset));
            }
        }
    }
    anchors
}

/// Override edges for the reachability BFS, keyed base-method FQN → the
/// overriding methods' anchors: a reachable base method makes its overrides
/// reachable, recovering the polymorphic call the graph misses (a base-typed
/// `x.method()` resolves to the base, not each concrete override). Shared so
/// `deadcode` (liveness) and `canonical` (reached-by-a-test) propagate
/// polymorphism identically.
pub fn override_edges(
    hier: &Hierarchy,
    def_anchor: &HashMap<String, (String, u32)>,
) -> HashMap<String, Vec<(String, u32)>> {
    let mut edges: HashMap<String, Vec<(String, u32)>> = HashMap::new();
    for class_fqn in hier.class_fqns() {
        if let Some(ms) = hier.methods(class_fqn) {
            for m in ms {
                let Some(anchor) = def_anchor.get(&format!("{class_fqn}.{m}")) else {
                    continue;
                };
                for base in hier.overrides(class_fqn, m) {
                    edges
                        .entry(format!("{base}.{m}"))
                        .or_default()
                        .push(anchor.clone());
                }
            }
        }
    }
    edges
}

/// Files whose every top-level callable is framework/convention entry glue —
/// flagging anything in them dead is unsafe, so they seed the roots wholesale.
/// Public so `canonical` can keep these out of the most-used ranking: a helper
/// in `scripts/`, `manage.py`, `urls.py`, a migration or a management command is
/// glue, not a reusable utility to reach for.
pub fn is_entrypoint_file(path: &str) -> bool {
    let base = Path::new(path)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(path);
    let comps: Vec<&str> = path.split(['/', '\\']).collect();
    matches!(
        base,
        "manage.py" | "wsgi.py" | "asgi.py" | "conftest.py" | "setup.py" | "urls.py" | "settings.py"
    ) || comps.contains(&"migrations")
        || comps.contains(&"settings")
        || windowed_contains(&comps, "management", "commands")
        || comps.first() == Some(&"scripts")
        || comps.first() == Some(&"bin")
}

/// Whether `a` is immediately followed by `b` in `comps` (a path subsequence).
fn windowed_contains(comps: &[&str], a: &str, b: &str) -> bool {
    comps.windows(2).any(|w| w[0] == a && w[1] == b)
}

/// Whether a file is a *runnable script* — meant to be invoked on its own, not
/// part of the long-running app. The `inputs` verb uses this to keep per-script
/// CLI args / env reads out of the default (app-surface) view; a script's inputs
/// show only when that script is queried by name.
///
/// This is the runnable *subset* of [`is_entrypoint_file`] — deliberately
/// narrower: `settings.py`, `wsgi.py`/`asgi.py`, `urls.py`, and `migrations/`
/// are the app's own config/launch surface, **not** scripts. A file qualifies
/// when it is a Django management command, lives in a `scripts/`/`bin/` tree,
/// is `manage.py`, or carries a `if __name__ == "__main__":` guard.
pub fn is_script_file(path: &str, has_main_guard: bool) -> bool {
    let base = Path::new(path)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(path);
    if base == "__init__.py" {
        return false;
    }
    let comps: Vec<&str> = path.split(['/', '\\']).collect();
    has_main_guard
        || base == "manage.py"
        || windowed_contains(&comps, "management", "commands")
        || comps.first() == Some(&"scripts")
        || comps.first() == Some(&"bin")
}

/// Whether a console-script target (`module.func`, as written in pyproject)
/// names the def with this FQN — exact, or by suffix to tolerate a source root
/// (pyproject's `pkg.cli` vs the file-derived `src.pkg.cli`).
fn matches_console(target: &str, fqn: &str) -> bool {
    fqn == target || fqn.ends_with(&format!(".{target}"))
}

/// Console entrypoint targets from `<root>/pyproject.toml` — `[project.scripts]`
/// and `[tool.poetry.scripts]` values like `"pkg.cli:main"`, returned as
/// `pkg.cli.main`. Best-effort: any read/parse failure yields none.
fn console_script_targets(root: &str) -> Vec<String> {
    let path = Path::new(root).join("pyproject.toml");
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let Ok(doc) = text.parse::<toml::Table>() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut harvest = |table: Option<&toml::Value>| {
        if let Some(toml::Value::Table(t)) = table {
            for v in t.values() {
                if let Some(s) = v.as_str() {
                    // "pkg.mod:func" → "pkg.mod.func"; drop any ":obj.attr" tail.
                    if let Some((module, attr)) = s.split_once(':') {
                        let func = attr.split('.').next().unwrap_or(attr);
                        out.push(format!("{module}.{func}"));
                    }
                }
            }
        }
    };
    harvest(doc.get("project").and_then(|p| p.get("scripts")));
    harvest(
        doc.get("tool")
            .and_then(|t| t.get("poetry"))
            .and_then(|p| p.get("scripts")),
    );
    out
}

#[cfg(test)]
mod tests {
    use super::is_script_file;

    #[test]
    fn classifies_runnable_scripts_but_not_app_config() {
        // Runnable scripts.
        assert!(is_script_file("manage.py", false));
        assert!(is_script_file("app/management/commands/backfill.py", false));
        assert!(is_script_file("scripts/seed.py", false));
        assert!(is_script_file("bin/run.py", false));
        // A `__main__`-guarded module anywhere is a script.
        assert!(is_script_file("pkg/tool.py", true));

        // App config / launch surface is NOT a script — `inputs` shows it by
        // default. This is the distinction that separates this from
        // `is_entrypoint_file`, which lumps them together.
        assert!(!is_script_file("salessync/settings.py", false));
        assert!(!is_script_file("salessync/wsgi.py", false));
        assert!(!is_script_file("salessync/asgi.py", false));
        assert!(!is_script_file("api/urls.py", false));
        assert!(!is_script_file("api/migrations/0001_initial.py", false));
        assert!(!is_script_file("api/services/email_service.py", false));

        // A package marker is never a script, even under a commands/ tree.
        assert!(!is_script_file("app/management/commands/__init__.py", false));
    }
}
