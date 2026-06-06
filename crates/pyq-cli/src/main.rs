//! `pyq` — a queryable static index for Python codebases.
//!
//! Verb-per-invocation CLI (`pyq <verb> [args] [--json]`), mirroring the design
//! pyq is built around: a token-frugal oracle an agent queries for ground truth
//! about code-as-graph. This is the first slice — symbol/reference queries over
//! a directory of Python files, single-file name resolution.

mod graph;
mod walk;

use clap::{Parser, Subcommand};
use pyq_index::{extract, DefKind, FileIndex, InputKind};
use pyq_output::{Channel, Envelope};
use pyq_resolve::{Loc, Resolver, TyResolver};
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

    /// Use the single-file syntactic extractor instead of ty's cross-file
    /// engine. For comparison/fallback; ty is the default for refs/defs.
    #[arg(long, global = true)]
    syntactic: bool,
}

#[derive(Subcommand)]
enum Command {
    /// Every reference to a symbol (reads and calls) across the tree.
    Refs { symbol: String },
    /// Every call site of a symbol (`name(...)`).
    Callers { symbol: String },
    /// Every definition of a symbol (function/class/variable/import binding).
    Defs { symbol: String },
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
        Command::Refs { symbol } | Command::Callers { symbol } | Command::Defs { symbol } => {
            Some(symbol.as_str())
        }
        _ => None,
    };
    if matches!(symbol, Some(s) if s.trim().is_empty()) {
        anyhow::bail!("symbol must not be empty");
    }

    // `inputs` and any verb under `--syntactic` use the single-file extractor;
    // `refs`/`callers`/`defs` otherwise route through ty's cross-file engine
    // behind the Resolver trait.
    match &cli.command {
        Command::Inputs => {
            let files = walk::index_tree(&cli.root)?;
            Ok(query_inputs(&files))
        }
        Command::Imports {
            module,
            reverse,
            cycles,
        } => {
            let files = walk::index_tree(&cli.root)?;
            Ok(query_imports(&files, module.as_deref(), *reverse, *cycles))
        }
        Command::Refs { symbol } if cli.syntactic => {
            let files = walk::index_tree(&cli.root)?;
            Ok(query_refs(&files, symbol, None))
        }
        Command::Callers { symbol } if cli.syntactic => {
            let files = walk::index_tree(&cli.root)?;
            Ok(query_refs(&files, symbol, Some(true)))
        }
        Command::Defs { symbol } if cli.syntactic => {
            let files = walk::index_tree(&cli.root)?;
            Ok(query_defs(&files, symbol))
        }
        Command::Refs { symbol } => resolved(&cli.root, symbol, "refs", |r, s| r.references(s)),
        Command::Callers { symbol } => resolved(&cli.root, symbol, "callers", |r, s| r.callers(s)),
        Command::Defs { symbol } => resolved(&cli.root, symbol, "defs", |r, s| r.definitions(s)),
    }
}

/// Run a ty-backed resolver query and build the envelope.
fn resolved(
    root: &str,
    symbol: &str,
    kind: &str,
    query: fn(&TyResolver, &str) -> anyhow::Result<Vec<Loc>>,
) -> anyhow::Result<Envelope> {
    let resolver = TyResolver::new(root, walk::walked_py_files(root))?;
    let locs = query(&resolver, symbol)?;
    let results = locs.iter().map(loc_to_json).collect::<Vec<_>>();
    let summary = format!("{} {} of `{}` (ty, cross-file)", results.len(), kind, symbol);
    Ok(
        Envelope::new(json!({ "kind": kind, "symbol": symbol, "engine": "ty" }), results)
            .with_summary(summary),
    )
}

fn loc_to_json(loc: &Loc) -> serde_json::Value {
    json!({
        "loc": format!("{}:{}:{}", loc.path, loc.line, loc.col),
        "label": loc.kind,
    })
}

/// References to `symbol`. `calls_only = Some(true)` restricts to call sites.
fn query_refs(files: &[FileIndex], symbol: &str, calls_only: Option<bool>) -> Envelope {
    let mut results = Vec::new();
    for f in files {
        for r in &f.refs {
            if r.name != symbol {
                continue;
            }
            if calls_only == Some(true) && !r.is_call {
                continue;
            }
            results.push(json!({
                "loc": format!("{}:{}:{}", f.path, r.pos.line, r.pos.col),
                "label": if r.is_call { "call" } else { "ref" },
            }));
        }
    }
    let kind = if calls_only == Some(true) { "callers" } else { "refs" };
    let summary = format!("{} {} of `{}`", results.len(), kind, symbol);
    Envelope::new(json!({ "kind": kind, "symbol": symbol }), results).with_summary(summary)
}

/// Definitions of `symbol`.
fn query_defs(files: &[FileIndex], symbol: &str) -> Envelope {
    let mut results = Vec::new();
    for f in files {
        for d in &f.defs {
            if d.name != symbol {
                continue;
            }
            results.push(json!({
                "loc": format!("{}:{}:{}", f.path, d.pos.line, d.pos.col),
                "label": format!("{}{}", def_kind_str(d.kind), if d.nested { " (nested)" } else { "" }),
            }));
        }
    }
    let summary = format!("{} definitions of `{}`", results.len(), symbol);
    Envelope::new(json!({ "kind": "defs", "symbol": symbol }), results).with_summary(summary)
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
    Envelope::new(json!({ "kind": "inputs" }), results).with_summary(summary)
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
        return Envelope::new(json!({ "kind": "imports", "mode": "cycles" }), results)
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
            let m = graph::normalize_query(arg);
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
    let mut query = json!({ "kind": "imports", "mode": mode });
    if let Some(t) = target {
        query["target"] = json!(t);
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

fn def_kind_str(kind: DefKind) -> &'static str {
    match kind {
        DefKind::Function => "function",
        DefKind::Class => "class",
        DefKind::Variable => "variable",
        DefKind::Import => "import",
    }
}

/// Re-export for the walk module.
pub(crate) fn extract_file(path: &str, source: &str) -> FileIndex {
    extract(path, source)
}
