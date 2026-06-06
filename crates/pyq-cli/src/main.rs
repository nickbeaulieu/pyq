//! `pyq` — a queryable static index for Python codebases.
//!
//! Verb-per-invocation CLI (`pyq <verb> [args] [--json]`), mirroring the design
//! pyq is built around: a token-frugal oracle an agent queries for ground truth
//! about code-as-graph. This is the first slice — symbol/reference queries over
//! a directory of Python files, single-file name resolution.

mod graph;
mod walk;

use clap::{Parser, Subcommand};
use pyq_index::{extract, EffectKind, FileIndex, InputKind};
use pyq_output::{Channel, Envelope};
use pyq_resolve::{scope_fqn, CallGraph, Direction, GraphNode, Loc, Resolver, UnifiedResolver};
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
    /// The external input surface — env vars, literal files opened, CLI args,
    /// and pydantic settings fields. "What does this need to run."
    Inputs,
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
