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

use crate::tests_map::{is_test_file, test_class_fqns};

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
    let hier = crate::hierarchy::Hierarchy::build(files, graph);

    // Classes whose every method is framework-invoked (so a method's presence in
    // the call graph isn't required for it to be live): a class in an entrypoint
    // file, a collected test class, or — the general signal — one that extends a
    // base ty can't resolve to a first-party class (Django/DRF/stdlib/uninstalled).
    // The last subsumes the old curated suffix list AND a project's own
    // framework usage, and is what fixes the polymorphic-override false positives
    // (a `BasePermission` subclass's `has_permission` is live by inheritance).
    let mut entry_classes: HashSet<String> = test_classes.clone();
    for f in files {
        for d in &f.defs {
            if d.kind == DefKind::Class {
                let fqn = scope_fqn(&f.path, &class_scope(d));
                if is_entrypoint_file(&f.path) || hier.has_external_ancestor(&fqn) {
                    entry_classes.insert(fqn);
                }
            }
        }
    }

    // Names defined anywhere in the project — a cheap filter so we only pay ty to
    // resolve a module-scope reference that *could* name a first-party callable.
    let def_names: HashSet<&str> = files
        .iter()
        .flat_map(|f| f.defs.iter().map(|d| d.name.as_str()))
        .collect();

    // FQN → anchor for every callable, so a dotted-string config path
    // (`'api.utils.handler'`) can be resolved to a real def and seeded.
    let mut def_anchor: HashMap<String, (String, u32)> = HashMap::new();
    for f in files {
        for d in &f.defs {
            if matches!(d.kind, DefKind::Function | DefKind::Class) {
                def_anchor
                    .entry(scope_fqn(&f.path, &def_scope(d)))
                    .or_insert((f.path.clone(), d.offset));
            }
        }
    }

    // Collect root anchors: (path, name offset) of every callable that is live by
    // entry. Deduped because a def can satisfy several rules.
    let mut seeds: HashSet<(String, u32)> = HashSet::new();
    for f in files {
        let entry_file = is_entrypoint_file(&f.path);
        for d in &f.defs {
            if !matches!(d.kind, DefKind::Function | DefKind::Class) {
                continue;
            }
            let fqn = scope_fqn(&f.path, &def_scope(d));
            // Live if this def, or any class enclosing it, is an entrypoint
            // class — so a framework class, its methods, and its inner config
            // classes (`Meta`, `Config`) are all live as one managed unit.
            let in_entry_class = (1..=d.container.len())
                .any(|k| entry_classes.contains(&scope_fqn(&f.path, &d.container[..k])));
            let is_entry_class = d.kind == DefKind::Class && entry_classes.contains(&fqn);
            let is_root = d.decorated
                || is_dunder(&d.name)
                || entry_file
                || is_pytest_test(d, &f.path, &test_classes)
                || in_entry_class
                || is_entry_class
                || (d.container.is_empty() && f.dunder_all.contains(&d.name))
                || console.iter().any(|t| matches_console(t, &fqn));
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
    // the base, not each concrete override). Keyed base-method FQN → the
    // overriding methods' anchors; the BFS folds them in as extra successors.
    let mut override_edges: HashMap<String, Vec<(String, u32)>> = HashMap::new();
    for class_fqn in hier.class_fqns() {
        if let Some(ms) = hier.methods(class_fqn) {
            for m in ms {
                let Some(anchor) = def_anchor.get(&format!("{class_fqn}.{m}")) else {
                    continue;
                };
                for base in hier.overrides(class_fqn, m) {
                    override_edges
                        .entry(format!("{base}.{m}"))
                        .or_default()
                        .push(anchor.clone());
                }
            }
        }
    }

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

/// Whether a def is a pytest-collected test: a `test_*` function in a test file,
/// or a `test_*` method on a collected test class.
fn is_pytest_test(d: &Def, path: &str, test_classes: &HashSet<String>) -> bool {
    if !is_test_file(path) || d.kind != DefKind::Function || !d.name.starts_with("test") {
        return false;
    }
    match d.container.last() {
        // A method: its enclosing class must be one pytest collects.
        Some(_) => test_classes.contains(&scope_fqn(path, &d.container)),
        // A free function in a test file.
        None => true,
    }
}

/// Files whose every top-level callable is framework/convention entry glue —
/// flagging anything in them dead is unsafe, so they seed the roots wholesale.
fn is_entrypoint_file(path: &str) -> bool {
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
