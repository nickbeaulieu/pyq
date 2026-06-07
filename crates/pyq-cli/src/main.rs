//! `pyq` — a queryable static index for Python codebases.
//!
//! Verb-per-invocation CLI (`pyq <verb> [args] [--json]`), mirroring the design
//! pyq is built around: a token-frugal oracle an agent queries for ground truth
//! about code-as-graph. This is the first slice — symbol/reference queries over
//! a directory of Python files, single-file name resolution.

mod deadcode;
mod graph;
mod hierarchy;
mod mock;
mod tests_map;
mod walk;

use clap::{Parser, Subcommand};
use pyq_dynamic::{observed_effects, default_python, TraceOptions};
use pyq_index::{extract, EffectKind, FileIndex, InputKind};
use pyq_output::{Channel, Envelope};
use pyq_resolve::{
    scope_fqn, CallGraph, Direction, GraphNode, Loc, Resolver, UnifiedResolver,
};
use std::collections::{BTreeSet, HashSet};
use serde_json::json;
use std::process::ExitCode;

#[derive(Parser)]
#[command(name = "pyq", version, about = "Queryable static index for Python")]
struct Cli {
    #[command(subcommand)]
    command: Command,

    /// Emit the compact JSON envelope instead of the human view.
    #[arg(long, global = true)]
    json: bool,

    /// Emit indented JSON.
    #[arg(long, global = true)]
    pretty: bool,

    /// Root directory to scan (defaults to the current directory).
    #[arg(long, global = true, default_value = ".")]
    root: String,
}

#[derive(Subcommand)]
enum Command {
    /// Every reference to a symbol (reads and calls) across the tree.
    Refs { symbol: String },
    /// Every call site of a symbol (`name(...)`).
    Callers { symbol: String },
    /// Every definition of a symbol (function/class/variable/import binding).
    Defs { symbol: String },
    /// The transitive call graph of a symbol, over stable fully-qualified IDs.
    /// Default: the forward closure (everything it transitively calls). With
    /// `--reverse`: everything that transitively calls it. `--depth N` caps the
    /// hops. Accepts a bare name, a qualified name, or a full FQN.
    Graph {
        symbol: String,
        /// Walk callers (who reaches this) instead of callees (what it reaches).
        #[arg(long)]
        reverse: bool,
        /// Maximum transitive depth (default: unbounded).
        #[arg(long)]
        depth: Option<usize>,
    },
    /// The transitive effect surface of a symbol: which side effects (files,
    /// network, subprocess, env, db, randomness, clock, global mutation) it and
    /// everything it transitively calls statically touch — plus import-time
    /// effects of the modules involved. "Is this pure / safe in a test."
    Effects { symbol: String },
    /// Which tests statically reach a symbol — the test↔code map. Projects the
    /// reverse call graph and keeps the callers pytest would collect as tests
    /// (`test_*` functions in `test_*.py`/`*_test.py`, `test_*` methods on
    /// `Test*` classes). Each test carries the call path (`via`) and `depth` by
    /// which it reaches the symbol. The foundation for static change coverage.
    Tests { symbol: String },
    /// The external input surface — env vars, literal files opened, CLI args,
    /// and pydantic settings fields. "What does this need to run."
    Inputs,
    /// Resolve every `mock.patch("a.b.c")` target against the project and flag
    /// drifted paths — a patch whose target no longer exists silently does
    /// nothing, so the test passes while exercising the real code.
    MockTargets,
    /// The class hierarchy of a symbol: its supertypes (bases, external marked)
    /// and transitive subclasses, plus the override map — which base methods it
    /// overrides and which subclasses override its methods. The OO-refactor
    /// footgun, as data. Accepts a bare/qualified class name.
    Hierarchy { symbol: String },
    /// Functions and classes reachable from no entrypoint — candidate dead code.
    /// Reachability runs forward from the roots an agent can't see are live:
    /// tests, dunders, decorated hooks, `__all__`, module-scope calls, framework
    /// entrypoint files/classes, and console scripts. Over-approximate liveness
    /// (so it under-reports death); residual dynamic dispatch is flagged.
    Deadcode,
    /// The project import graph. With no module: every import edge. With a
    /// module (`pkg.models` or `pkg/models.py`): what it imports, or — with
    /// `--reverse` — who imports it (blast radius). `--cycles`: import cycles.
    Imports {
        /// Module or file to query. Omit to list every edge.
        module: Option<String>,
        /// Show who imports the module (reverse deps) instead of what it imports.
        #[arg(long)]
        reverse: bool,
        /// Report import cycles among project modules.
        #[arg(long)]
        cycles: bool,
    },
    /// Run the project's test suite under the dynamic tier and report the side
    /// effects it *actually* performs at runtime — the runtime oracle that
    /// confirms/refutes the static `effects` verb on its dynamic-dispatch blind
    /// spot. Keyed by the same FQNs, so the two join (effect-diff, #9.3). Runs
    /// your tests: invoking it is consent, the same as typing `pytest`.
    Trace {
        /// Arguments passed through to pytest (test paths, `-k`, `-m`, …).
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        pytest_args: Vec<String>,
        /// Python interpreter to drive (defaults to `$PYQ_PYTHON` or `python3`).
        #[arg(long)]
        python: Option<String>,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let channel = if cli.json {
        Channel::Json
    } else if cli.pretty {
        Channel::Pretty
    } else {
        Channel::Human
    };

    let envelope = match dispatch(&cli) {
        Ok(env) => env,
        Err(e) => {
            eprintln!("pyq: {e:#}");
            return ExitCode::FAILURE;
        }
    };

    println!("{}", envelope.render(channel).trim_end());
    ExitCode::SUCCESS
}

fn dispatch(cli: &Cli) -> anyhow::Result<Envelope> {
    // A blank symbol is a usage error, not a 0-result success that reads as
    // "this name is unused."
    let symbol = match &cli.command {
        Command::Refs { symbol }
        | Command::Callers { symbol }
        | Command::Defs { symbol }
        | Command::Graph { symbol, .. }
        | Command::Tests { symbol }
        | Command::Hierarchy { symbol }
        | Command::Effects { symbol } => Some(symbol.as_str()),
        _ => None,
    };
    if matches!(symbol, Some(s) if s.trim().is_empty()) {
        anyhow::bail!("symbol must not be empty");
    }

    // One query path. `inputs`/`imports` are pure syntactic facts; for
    // `refs`/`callers`/`defs` the Resolver trait merges ty (authoritative,
    // cross-file) with the syntactic scan (ty's blind spots) into one answer.
    let mut envelope = match &cli.command {
        Command::Inputs => {
            let files = walk::index_tree(&cli.root)?;
            query_inputs(&files)
        }
        Command::MockTargets => query_mock_targets(cli)?,
        Command::Hierarchy { symbol } => query_hierarchy(cli, symbol)?,
        Command::Deadcode => query_deadcode(cli)?,
        Command::Imports {
            module,
            reverse,
            cycles,
        } => {
            let files = walk::index_tree(&cli.root)?;
            query_imports(&files, module.as_deref(), *reverse, *cycles)
        }
        Command::Refs { symbol } => resolve(cli, symbol, "refs", |r, s| r.references(s))?,
        Command::Callers { symbol } => resolve(cli, symbol, "callers", |r, s| r.callers(s))?,
        Command::Defs { symbol } => resolve(cli, symbol, "defs", |r, s| r.definitions(s))?,
        Command::Graph {
            symbol,
            reverse,
            depth,
        } => query_graph(cli, symbol, *reverse, *depth)?,
        Command::Effects { symbol } => query_effects(cli, symbol)?,
        Command::Tests { symbol } => query_tests(cli, symbol)?,
        Command::Trace {
            pytest_args,
            python,
        } => query_trace(cli, pytest_args, python.as_deref())?,
    };

    // Anchor every result to one resolved root, echoed in the query — so the
    // same logical query gives the same answer (and the same paths) from any
    // working directory.
    if let Some(obj) = envelope.query.as_object_mut() {
        let root = std::fs::canonicalize(&cli.root)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| cli.root.clone());
        obj.insert("root".to_string(), json!(root));
    }
    Ok(envelope)
}

/// The dynamic tier: run the suite under the bundled sidecar and report the
/// effects observed at runtime. All interpreter/subprocess contact lives in
/// `pyq-dynamic`; here we only translate flags and let the resulting envelope
/// flow through the same rendering path as every static verb.
fn query_trace(
    cli: &Cli,
    pytest_args: &[String],
    python: Option<&str>,
) -> anyhow::Result<Envelope> {
    let mut opts = TraceOptions::new(cli.root.clone());
    opts.python = python.map(str::to_string).unwrap_or_else(default_python);
    opts.pytest_args = pytest_args.to_vec();
    observed_effects(&opts)
}

/// Run one symbol query through the resolver and build the envelope. One engine,
/// one answer — the caller never sees ty vs. the syntactic locator behind it.
fn resolve(
    cli: &Cli,
    symbol: &str,
    kind: &str,
    query: fn(&dyn Resolver, &str) -> anyhow::Result<Vec<Loc>>,
) -> anyhow::Result<Envelope> {
    let files = walk::index_tree(&cli.root)?;
    let scope = walk::walked_py_files(&cli.root);
    let resolver = UnifiedResolver::new(&cli.root, files, scope)?;
    let locs = query(&resolver, symbol)?;
    let results = locs.iter().map(loc_to_json).collect::<Vec<_>>();
    let summary = format!("{} {} of `{}`", results.len(), kind, symbol);
    Ok(Envelope::new(json!({ "kind": kind, "target": symbol }), results).with_summary(summary))
}

fn loc_to_json(loc: &Loc) -> serde_json::Value {
    let mut v = json!({
        "loc": format!("{}:{}:{}", loc.path, loc.line, loc.col),
        "label": loc.kind,
        "role": loc.role,
    });
    if let Some(target) = &loc.resolves_to {
        v["resolves_to"] = json!(target);
    }
    v
}

/// The transitive call graph of a symbol — forward (callees) or, with
/// `--reverse`, backward (callers) closure over stable fully-qualified IDs.
fn query_graph(
    cli: &Cli,
    symbol: &str,
    reverse: bool,
    depth: Option<usize>,
) -> anyhow::Result<Envelope> {
    let files = walk::index_tree(&cli.root)?;
    let scope = walk::walked_py_files(&cli.root);
    let graph = CallGraph::new(&cli.root, files, scope)?;
    let dir = if reverse {
        Direction::Reverse
    } else {
        Direction::Forward
    };
    let closure = graph.closure(symbol, dir, depth);

    let results = closure.nodes.iter().map(node_to_json).collect::<Vec<_>>();
    let n = results.len();
    let summary = if reverse {
        format!("{n} {} transitively reach `{symbol}`", plural(n, "node"))
    } else {
        format!("{n} {} reachable from `{symbol}`", plural(n, "node"))
    };
    // Echo the resolved FQN roots: the durable handle(s) the symbol mapped to,
    // re-queryable after edits even when line numbers move.
    let query = json!({
        "kind": "graph",
        "mode": if reverse { "reverse" } else { "forward" },
        "target": symbol,
        "roots": closure.roots,
    });
    let mut envelope = Envelope::new(query, results).with_summary(summary);
    // No root means the symbol named no function or class — a 0-result graph
    // that must not read as "found, but isolated."
    if closure.roots.is_empty() {
        envelope = envelope
            .with_warnings(vec![format!("no function or class named `{symbol}` found")]);
    }
    Ok(envelope)
}

/// The static test↔code map: which collected tests transitively reach `symbol`.
/// A projection of the reverse call-graph closure, filtered to test nodes — a
/// call-reachability lens for "which tests to run before this edit," not a
/// coverage metric (dynamic dispatch is invisible; see `tests_map`).
fn query_tests(cli: &Cli, symbol: &str) -> anyhow::Result<Envelope> {
    let files = walk::index_tree(&cli.root)?;
    let scope = walk::walked_py_files(&cli.root);
    // The classes pytest collects (Test*-named or *TestCase-subclassing) — built
    // from the index before `files` is moved into the graph, since a graph node
    // alone carries no base-class info.
    let test_classes = tests_map::test_class_fqns(&files);
    let graph = CallGraph::new(&cli.root, files, scope)?;
    let closure = graph.closure(symbol, Direction::Reverse, None);

    // A test may be a root (it reaches the symbol directly) only if the symbol
    // itself is a test — the roots are the queried symbol, not its callers — so
    // reaching tests come from the closure's reached nodes.
    let mut tests: Vec<&GraphNode> = closure
        .nodes
        .iter()
        .filter(|n| tests_map::is_test_node(n, &test_classes))
        .collect();
    tests.sort_by(|a, b| (a.depth, &a.fqn).cmp(&(b.depth, &b.fqn)));

    let results: Vec<serde_json::Value> = tests
        .iter()
        .map(|n| {
            json!({
                "loc": format!("{}:{}:{}", n.path, n.line, n.col),
                "label": format!("{} reaches `{symbol}` (depth {}, via {})", n.fqn, n.depth, n.via),
                "fqn": n.fqn,
                "depth": n.depth,
                "via": n.via,
            })
        })
        .collect();

    let summary = tests_map::summary(symbol, closure.roots.is_empty(), results.len());
    let query = json!({
        "kind": "tests",
        "target": symbol,
        "roots": closure.roots,
    });
    let mut envelope = Envelope::new(query, results).with_summary(summary);

    let mut warnings = Vec::new();
    if closure.roots.is_empty() {
        warnings.push(format!("no function or class named `{symbol}` found"));
    } else {
        // Honest boundary: reachability is the call graph's static
        // over-approximation (dynamic/attribute dispatch not followed), and test
        // collection uses pytest's default naming/location rules — custom
        // `python_files`/`python_classes` config is not read.
        warnings.push(
            "static over-approximation: tests reaching via dynamic/attribute dispatch are not followed; test collection uses pytest naming + unittest/TestCase-inheritance rules (custom python_files/python_classes config not read)".to_string(),
        );
    }
    envelope = envelope.with_warnings(warnings);
    Ok(envelope)
}

fn node_to_json(node: &GraphNode) -> serde_json::Value {
    json!({
        "loc": format!("{}:{}:{}", node.path, node.line, node.col),
        "label": format!("{} {} (depth {}, via {})", node.kind, node.fqn, node.depth, node.via),
        "fqn": node.fqn,
        "node_kind": node.kind,
        "depth": node.depth,
        "via": node.via,
    })
}

/// The transitive effect surface of a symbol: the side effects performed by the
/// symbol and everything it transitively calls (forward call closure), plus the
/// import-time effects of every module that contributes a reachable callable.
fn query_effects(cli: &Cli, symbol: &str) -> anyhow::Result<Envelope> {
    let files = walk::index_tree(&cli.root)?;
    let scope = walk::walked_py_files(&cli.root);
    let graph = CallGraph::new(&cli.root, files.clone(), scope)?;
    let closure = graph.closure(symbol, Direction::Forward, None);

    // Everything reachable from the symbol, the roots included.
    let reachable: HashSet<String> = closure
        .roots
        .iter()
        .cloned()
        .chain(closure.nodes.iter().map(|n| n.fqn.clone()))
        .collect();
    // A file is "in play" if it defines a reachable callable — importing such a
    // module runs its import-time effects, so we surface those too.
    let in_play: HashSet<&str> = files
        .iter()
        .filter(|f| {
            f.defs.iter().any(|d| {
                matches!(d.kind, pyq_index::DefKind::Function | pyq_index::DefKind::Class)
                    && reachable.contains(&owner_fqn(&f.path, &d.container, &d.name))
            })
        })
        .map(|f| f.path.as_str())
        .collect();

    let mut rows: Vec<(String, serde_json::Value, &'static str)> = Vec::new();
    let mut categories: BTreeSet<&'static str> = BTreeSet::new();
    for f in &files {
        for e in &f.effects {
            let owner = scope_fqn(&f.path, &e.scope);
            let keep = if e.import_time {
                in_play.contains(f.path.as_str())
            } else {
                reachable.contains(&owner)
            };
            if !keep {
                continue;
            }
            let cat = effect_kind_str(e.kind);
            categories.insert(cat);
            let loc = format!("{}:{}:{}", f.path, e.pos.line, e.pos.col);
            let in_label = if e.import_time {
                format!("{} (import-time)", module_label(&f.path))
            } else {
                owner.clone()
            };
            let label = format!("{cat} {}  in {in_label}", e.detail);
            rows.push((
                loc.clone(),
                json!({
                    "loc": loc,
                    "label": label,
                    "effect": cat,
                    "api": e.detail,
                    "owner": owner,
                    "import_time": e.import_time,
                }),
                cat,
            ));
        }
    }

    rows.sort_by(|a, b| (a.2, &a.0).cmp(&(b.2, &b.0)));
    rows.dedup_by(|a, b| a.0 == b.0 && a.1["api"] == b.1["api"]);
    let results: Vec<serde_json::Value> = rows.iter().map(|(_, v, _)| v.clone()).collect();

    let cats: Vec<&str> = categories.iter().copied().collect();
    let summary = if closure.roots.is_empty() {
        format!("no function or class named `{symbol}` found")
    } else if results.is_empty() {
        format!(
            "`{symbol}` is pure — no static effects across {} reachable {}",
            reachable.len(),
            plural(reachable.len(), "callable")
        )
    } else {
        format!(
            "effects of `{symbol}`: {} — {} {}",
            cats.join(", "),
            results.len(),
            plural(results.len(), "site")
        )
    };
    let query = json!({ "kind": "effects", "target": symbol, "categories": cats });
    let mut envelope = Envelope::new(query, results).with_summary(summary);

    let mut warnings = Vec::new();
    if closure.roots.is_empty() {
        warnings.push(format!("no function or class named `{symbol}` found"));
    } else {
        // Honest about the boundary: detection is syntactic/over-approximate, and
        // effects behind calls ty couldn't resolve (attribute/dynamic dispatch)
        // aren't reached — so "pure" means "no effect found", not "proven pure."
        warnings.push(
            "static over-approximation: effects behind dynamic/attribute-dispatched calls are not followed".to_string(),
        );
    }
    envelope = envelope.with_warnings(warnings);
    Ok(envelope)
}

/// The owner FQN of a def (`module + container + name`) — the call-graph node id.
fn owner_fqn(path: &str, container: &[String], name: &str) -> String {
    let mut scope = container.to_vec();
    scope.push(name.to_string());
    scope_fqn(path, &scope)
}

/// The module id of a file, for labelling import-time effects (`pkg/models.py`
/// → `pkg.models`).
fn module_label(path: &str) -> String {
    scope_fqn(path, &[])
}

fn effect_kind_str(kind: EffectKind) -> &'static str {
    match kind {
        EffectKind::Fs => "fs",
        EffectKind::Network => "network",
        EffectKind::Subprocess => "subprocess",
        EffectKind::Env => "env",
        EffectKind::Db => "db",
        EffectKind::Random => "random",
        EffectKind::Clock => "clock",
        EffectKind::GlobalState => "global",
    }
}

/// The external input surface across the tree (syntactic).
fn query_inputs(files: &[FileIndex]) -> Envelope {
    let mut results = Vec::new();
    for f in files {
        for i in &f.inputs {
            let kind = match i.kind {
                InputKind::Env => "env",
                InputKind::File => "file",
                InputKind::Arg => "arg",
                InputKind::Setting => "setting",
            };
            results.push(json!({
                "loc": format!("{}:{}:{}", f.path, i.pos.line, i.pos.col),
                "label": format!("{kind} {}", i.value),
            }));
        }
    }
    let summary = format!("{} inputs", results.len());
    // Uniform query schema: every verb echoes kind + target (null where none).
    Envelope::new(json!({ "kind": "inputs", "target": null }), results).with_summary(summary)
}

/// The class hierarchy of a symbol — supertypes, subclasses, and the override
/// map. A projection of the resolved inheritance graph.
fn query_hierarchy(cli: &Cli, symbol: &str) -> anyhow::Result<Envelope> {
    let files = walk::index_tree(&cli.root)?;
    let scope = walk::walked_py_files(&cli.root);
    let graph = CallGraph::new(&cli.root, files.clone(), scope)?;
    let h = hierarchy::Hierarchy::build(&files, &graph);

    // FQN → display location, for classes and methods (the override targets).
    let mut loc_of: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for f in &files {
        for d in &f.defs {
            if matches!(d.kind, pyq_index::DefKind::Class | pyq_index::DefKind::Function) {
                let mut scope = d.container.clone();
                scope.push(d.name.clone());
                loc_of.insert(
                    scope_fqn(&f.path, &scope),
                    format!("{}:{}:{}", f.path, d.pos.line, d.pos.col),
                );
            }
        }
    }

    // Resolve the symbol to the class FQN(s) it names: exact, dotted-suffix, or
    // bare leaf — the union, like the other symbol verbs.
    let roots: Vec<String> = h
        .class_fqns()
        .filter(|fqn| fqn_names(fqn, symbol))
        .cloned()
        .collect();

    let mut results = Vec::new();
    for root in &roots {
        let root_loc = loc_of.get(root).cloned().unwrap_or_default();
        // Supertypes — first-party bases, then external (framework) bases.
        for base in h.supers(root) {
            results.push(json!({
                "loc": loc_of.get(&base).cloned().unwrap_or_else(|| root_loc.clone()),
                "label": format!("supertype {base}"),
                "relation": "supertype", "fqn": base,
            }));
        }
        for ext in h.external_bases(root) {
            results.push(json!({
                "loc": root_loc,
                "label": format!("supertype {ext} (external)"),
                "relation": "supertype-external", "fqn": ext,
            }));
        }
        // Subclasses (transitive, first-party).
        for sub in h.subclasses(root) {
            results.push(json!({
                "loc": loc_of.get(&sub).cloned().unwrap_or_else(|| root_loc.clone()),
                "label": format!("subtype {sub}"),
                "relation": "subtype", "fqn": sub,
            }));
        }
        // Override map: methods of this class that override a base method, and
        // methods overridden by a subclass.
        if let Some(methods) = h.methods(root) {
            let mut names: Vec<&String> = methods.iter().collect();
            names.sort();
            for m in names {
                let mfqn = format!("{root}.{m}");
                for base in h.overrides(root, m) {
                    results.push(json!({
                        "loc": loc_of.get(&mfqn).cloned().unwrap_or_else(|| root_loc.clone()),
                        "label": format!("{mfqn} overrides {base}.{m}"),
                        "relation": "overrides", "fqn": format!("{base}.{m}"),
                    }));
                }
                for sub in h.overridden_by(root, m) {
                    results.push(json!({
                        "loc": loc_of.get(&format!("{sub}.{m}")).cloned().unwrap_or_else(|| root_loc.clone()),
                        "label": format!("{sub}.{m} overrides {mfqn}"),
                        "relation": "overridden-by", "fqn": format!("{sub}.{m}"),
                    }));
                }
            }
        }
    }

    let summary = if roots.is_empty() {
        format!("no class named `{symbol}` found")
    } else {
        format!("{} {} for `{symbol}`", results.len(), plural(results.len(), "relation"))
    };
    let mut env = Envelope::new(
        json!({ "kind": "hierarchy", "target": symbol, "roots": roots }),
        results,
    )
    .with_summary(summary);
    if roots.is_empty() {
        env = env.with_warnings(vec![format!("no class named `{symbol}` found")]);
    }
    Ok(env)
}

/// Whether `fqn` is named by `symbol` — exact, dotted-suffix (source-root
/// tolerant), or, when `symbol` is unqualified, by matching the leaf.
fn fqn_names(fqn: &str, symbol: &str) -> bool {
    fqn == symbol
        || fqn.ends_with(&format!(".{symbol}"))
        || (!symbol.contains('.') && fqn.rsplit('.').next() == Some(symbol))
}

/// Candidate dead code — callables reachable from no entrypoint. Builds the call
/// graph, seeds the entrypoint roots, and reports the callables the forward
/// closure never reaches.
fn query_deadcode(cli: &Cli) -> anyhow::Result<Envelope> {
    let files = walk::index_tree(&cli.root)?;
    let scope = walk::walked_py_files(&cli.root);
    let graph = CallGraph::new(&cli.root, files.clone(), scope)?;
    let result = deadcode::find(&files, &graph, &cli.root);

    let results: Vec<serde_json::Value> = result
        .dead
        .iter()
        .map(|d| {
            json!({
                "loc": format!("{}:{}:{}", d.path, d.line, d.col),
                "label": format!("{} {}", d.kind, d.fqn),
                "fqn": d.fqn,
                "node_kind": d.kind,
            })
        })
        .collect();
    let n = results.len();
    let summary = format!(
        "{n} dead-code {} of {} {}",
        plural(n, "candidate"),
        result.total,
        plural(result.total, "callable")
    );
    let warnings = vec![
        "over-approximate — verify before deleting. A candidate is reported when no \
         static call path reaches it from an entrypoint; it is NOT dead if it's reached \
         dynamically: a dotted-string path in config (Django `EXCEPTION_HANDLER`/`MIDDLEWARE`, \
         Celery task names), a callable passed as a value (`side_effect=`, callbacks, registries), \
         getattr/reflection, or a plugin/entry-point system pyq doesn't read."
            .to_string(),
    ];
    Ok(Envelope::new(json!({ "kind": "deadcode", "target": null }), results)
        .with_summary(summary)
        .with_warnings(warnings))
}

/// Every `mock.patch("...")` target across the tree, resolved against the
/// project. Drifted targets (a real project module, missing the looked-up name)
/// are the actionable signal — surfaced as warnings as well as results.
fn query_mock_targets(cli: &Cli) -> anyhow::Result<Envelope> {
    let files = walk::index_tree(&cli.root)?;
    let resolver = mock::PatchResolver::build(&files);
    // ty (via the call graph) lets the resolver follow an imported module into
    // typeshed/site-packages and resolve a method inherited from a project base;
    // best-effort, so a ty init failure falls back to the syntactic answer (those
    // cases stay `unverifiable`).
    let scope = walk::walked_py_files(&cli.root);
    let graph = CallGraph::new(&cli.root, files.clone(), scope).ok();
    let hier = graph
        .as_ref()
        .map(|g| hierarchy::Hierarchy::build(&files, g));
    let ctx = match (graph.as_ref(), hier.as_ref()) {
        (Some(graph), Some(hier)) => Some(mock::Ctx { graph, hier }),
        _ => None,
    };
    let mut rows: Vec<(String, serde_json::Value)> = Vec::new();
    let mut warnings = Vec::new();
    let mut drifted = 0usize;
    for f in &files {
        for m in &f.mocks {
            let status = resolver.resolve(m.target.as_deref(), ctx.as_ref());
            let loc = format!("{}:{}:{}", f.path, m.pos.line, m.pos.col);
            let shown = m.target.as_deref().unwrap_or("<dynamic>");
            let tag = status.tag();
            let detail = match &status {
                mock::Status::Drifted(why) | mock::Status::Unverifiable(why) => Some(why.clone()),
                _ => None,
            };
            let label = match &detail {
                Some(why) => format!("{tag} {shown} — {why}"),
                None => format!("{tag} {shown}"),
            };
            if matches!(status, mock::Status::Drifted(_)) {
                drifted += 1;
                warnings.push(format!("drifted patch `{shown}` ({loc}): {}", detail.clone().unwrap_or_default()));
            }
            rows.push((
                loc.clone(),
                json!({
                    "loc": loc,
                    "label": label,
                    "target": m.target,
                    "status": tag,
                }),
            ));
        }
    }
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    let results: Vec<serde_json::Value> = rows.into_iter().map(|(_, v)| v).collect();
    let n = results.len();
    let summary = if n == 0 {
        "no mock.patch targets".to_string()
    } else if drifted == 0 {
        format!("{n} patch {}, none drifted", plural(n, "target"))
    } else {
        format!(
            "{n} patch {}, {drifted} drifted",
            plural(n, "target")
        )
    };
    Ok(Envelope::new(json!({ "kind": "mock-targets", "target": null }), results)
        .with_summary(summary)
        .with_warnings(warnings))
}

/// The project import graph. Modes: cycles, reverse deps, forward deps, or —
/// with no module — every edge.
fn query_imports(
    files: &[FileIndex],
    module: Option<&str>,
    reverse: bool,
    cycles: bool,
) -> Envelope {
    let g = graph::Graph::build(files);

    if cycles {
        let mut results = Vec::new();
        for cycle in g.cycles() {
            let loc = g
                .file_of(&cycle[0])
                .map(|f| format!("{f}:1:1"))
                .unwrap_or_else(|| cycle[0].clone());
            // Ordered, closed path (a → b → … → a) so the edge to cut is visible.
            let mut path = cycle.clone();
            path.push(cycle[0].clone());
            results.push(json!({
                "loc": loc,
                "label": format!("cycle: {}", path.join(" → ")),
            }));
        }
        let summary = format!("{} import {}", results.len(), plural(results.len(), "cycle"));
        return Envelope::new(
            json!({ "kind": "imports", "mode": "cycles", "target": null }),
            results,
        )
        .with_summary(summary);
    }

    let mut rows: Vec<(String, serde_json::Value)> = Vec::new();
    // `target`/`found` are set only when a module is queried — `found` lets an
    // agent tell a typo'd module (not in the graph) from a real leaf with no
    // edges, so "0 importers" of a misspelling never reads as "safe to delete."
    let mut target: Option<String> = None;
    let mut found: Option<bool> = None;
    let (mode, summary): (&str, String) = match module {
        Some(arg) => {
            let m = g.resolve_module(&graph::normalize_query(arg));
            let known = g.knows(&m);
            found = Some(known);
            target = Some(m.clone());
            if reverse {
                for e in g.edges.iter().filter(|e| e.target == m) {
                    let loc = loc_str(&e.importer_file, e.pos);
                    rows.push((loc.clone(), json!({ "loc": loc, "label": format!("imported by {}", e.importer) })));
                }
                let summary = if !known {
                    format!("module `{m}` not found in project")
                } else {
                    format!("{} {} of `{}`", rows.len(), plural(rows.len(), "importer"), m)
                };
                ("reverse", summary)
            } else {
                for e in g.edges.iter().filter(|e| e.importer == m) {
                    let loc = loc_str(&e.importer_file, e.pos);
                    let tag = if e.internal { "" } else { " (ext)" };
                    rows.push((format!("{}{}", e.target, loc), json!({ "loc": loc, "label": format!("imports {}{}", e.target, tag) })));
                }
                let summary = if !known {
                    format!("module `{m}` not found in project")
                } else {
                    format!("`{}` imports {} {}", m, rows.len(), plural(rows.len(), "module"))
                };
                ("forward", summary)
            }
        }
        None => {
            for e in &g.edges {
                let loc = loc_str(&e.importer_file, e.pos);
                let tag = if e.internal { "" } else { " (ext)" };
                rows.push((loc.clone(), json!({ "loc": loc, "label": format!("{} → {}{}", e.importer, e.target, tag) })));
            }
            ("all", format!("{} import {}", rows.len(), plural(rows.len(), "edge")))
        }
    };

    rows.sort_by(|a, b| a.0.cmp(&b.0));
    let results = rows.into_iter().map(|(_, v)| v).collect::<Vec<_>>();
    // Uniform schema: kind + target (null for the whole-graph listing).
    let mut query = json!({ "kind": "imports", "mode": mode, "target": target });
    if let Some(found) = found {
        query["found"] = json!(found);
    }
    Envelope::new(query, results).with_summary(summary)
}

fn loc_str(file: &str, pos: pyq_index::Pos) -> String {
    format!("{}:{}:{}", file, pos.line, pos.col)
}

fn plural(n: usize, word: &str) -> String {
    if n == 1 {
        word.to_string()
    } else {
        format!("{word}s")
    }
}

/// Re-export for the walk module.
pub(crate) fn extract_file(path: &str, source: &str) -> FileIndex {
    extract(path, source)
}
