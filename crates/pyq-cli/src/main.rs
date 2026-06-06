//! `pyq` — a queryable static index for Python codebases.
//!
//! Verb-per-invocation CLI (`pyq <verb> [args] [--json]`), mirroring the design
//! pyq is built around: a token-frugal oracle an agent queries for ground truth
//! about code-as-graph. This is the first slice — symbol/reference queries over
//! a directory of Python files, single-file name resolution.

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
    /// The external input surface — env vars read and literal files opened.
    Inputs,
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
    // `callers` and `--syntactic` use the single-file extractor; everything else
    // routes through ty's cross-file engine behind the Resolver trait.
    match &cli.command {
        Command::Inputs => {
            let files = walk::index_tree(&cli.root)?;
            Ok(query_inputs(&files))
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
    let resolver = TyResolver::new(root)?;
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
