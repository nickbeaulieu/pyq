//! `pyq` — a queryable static index for Python codebases.
//!
//! Verb-per-invocation CLI (`pyq <verb> [args] [--json]`), mirroring the design
//! pyq is built around: a token-frugal oracle an agent queries for ground truth
//! about code-as-graph. This is the first slice — symbol/reference queries over
//! a directory of Python files, single-file name resolution.

mod cache;
mod canonical;
mod change_cov;
mod deadcode;
mod describe;
mod graph;
mod hierarchy;
mod mock;
mod tests_map;
mod walk;

use clap::{CommandFactory, FromArgMatches, Parser, Subcommand};
use pyq_dynamic::{
    default_python, observed_coverage, observed_effects, observed_shapes, TraceOptions,
};
use pyq_index::{extract, EffectKind, FileIndex, InputKind};
use pyq_output::{Channel, Envelope};
use pyq_resolve::{
    scope_fqn, CallGraph, Direction, GraphNode, Loc, Resolver, UnifiedResolver,
};
use std::collections::{BTreeSet, HashSet};
use serde_json::json;
use std::io::IsTerminal;
use std::process::ExitCode;

#[derive(Parser)]
#[command(
    name = "pyq",
    // Version carries the build's short commit sha (captured in build.rs), so
    // `pyq --version` pins exactly which build is running.
    version = concat!(env!("CARGO_PKG_VERSION"), " (", env!("PYQ_GIT_SHA"), ")"),
    about = "A queryable code graph for Python — who-calls, what-resolves, what-it-touches.",
    // The grouped, colorized top-level menu is assembled at runtime (see
    // `help_template`): clap has no native subcommand grouping, and color must be
    // gated on the terminal. Per-command detail still comes from each variant's
    // doc comment, shown by `pyq <command> --help`.
)]
struct Cli {
    #[command(subcommand)]
    command: Command,

    /// Emit the compact JSON envelope instead of the human view.
    #[arg(long, global = true)]
    json: bool,

    /// Emit indented JSON.
    #[arg(long, global = true)]
    pretty: bool,

    /// Root directory to scan (defaults to the current dir).
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
    /// One compact context pack for a symbol — signature, decorators, docstring
    /// line, and def line-span, plus its immediate callers, immediate callees,
    /// and the collected tests that reach it. The token-frugal "tell me about
    /// X": everything an agent would otherwise grep for, in one envelope.
    /// Accepts a bare name, a qualified name, or a full FQN.
    Describe { symbol: String },
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
    /// The repo's canonical surface, in one pass: its **most-used** helpers
    /// (internal callables ranked by how many distinct non-test callers use
    /// them — what to reach for, not reinvent), its **untested-public** surface
    /// (top-level public functions/classes no collected test statically
    /// reaches), and the **test** inventory (every collected test with its
    /// markers). The project-level "tell me about this codebase." Rows carry a
    /// `section`. Same dynamic-dispatch blind spot as the call graph.
    Canonical,
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
    /// Join the static effect surface against what the suite actually performs
    /// at runtime: `confirmed` (both), `dynamic-only` (runtime did it, static
    /// missed the edge — the dynamic-dispatch blind spot), `static-only` (static
    /// predicted it, runtime didn't — over-approximation or unexercised), and
    /// `unverifiable` (a category the audit hook can't see). Runs your tests.
    EffectDiff {
        /// Arguments passed through to pytest (test paths, `-k`, `-m`, …).
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        pytest_args: Vec<String>,
        /// Python interpreter to drive (defaults to `$PYQ_PYTHON` or `python3`).
        #[arg(long)]
        python: Option<String>,
    },
    /// Which lines changed since `--base` are exercised by the test suite, and
    /// by which tests — the runtime oracle behind the `tests` verb's "a static
    /// 0 is not 'untested'" caveat. Joins `git diff` against per-test line
    /// coverage (`sys.monitoring`, Python 3.12+; degrades on older). Runs your
    /// tests.
    ChangeCoverage {
        /// Git ref to diff against (default: `HEAD`, i.e. uncommitted changes).
        #[arg(long, default_value = "HEAD")]
        base: String,
        /// Arguments passed through to pytest (test paths, `-k`, `-m`, …).
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        pytest_args: Vec<String>,
        /// Python interpreter to drive (defaults to `$PYQ_PYTHON` or `python3`).
        #[arg(long)]
        python: Option<String>,
    },
    /// The concrete return type each callable actually produced at runtime —
    /// runtime evidence complementing ty's static inference, for spotting
    /// missing/loose annotations. Needs Python 3.12+ (`sys.monitoring`); runs
    /// your tests.
    Shapes {
        /// Arguments passed through to pytest (test paths, `-k`, `-m`, …).
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        pytest_args: Vec<String>,
        /// Python interpreter to drive (defaults to `$PYQ_PYTHON` or `python3`).
        #[arg(long)]
        python: Option<String>,
    },
}

fn main() -> ExitCode {
    // Parse through a command carrying the runtime-built menu, so `--help`
    // renders the grouped, color-gated overview (color must be decided here, not
    // baked into a static template).
    let command = Cli::command().help_template(help_menu(use_color()));
    let cli = match command.try_get_matches() {
        Ok(matches) => match Cli::from_arg_matches(&matches) {
            Ok(cli) => cli,
            Err(e) => e.exit(),
        },
        // clap exits 0 for --help/--version, non-zero for a usage error, after
        // printing the right thing — preserve that behavior.
        Err(e) => e.exit(),
    };
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

/// Whether to colorize the help menu: only when stdout is a real terminal and
/// the user hasn't opted out via `NO_COLOR` or a `dumb` terminal — so piped or
/// redirected help stays plain text.
fn use_color() -> bool {
    std::io::stdout().is_terminal()
        && std::env::var_os("NO_COLOR").is_none()
        && std::env::var("TERM").map(|t| t != "dumb").unwrap_or(true)
}

/// The grouped, bun-inspired top-level `--help` menu: a tagline, usage, the
/// verbs bucketed by use-case, the global flags, and a footer pinning the exact
/// build (version + commit sha). `clap` substitutes `{about}`/`{usage}`/
/// `{options}`; the rest is curated here so the menu reads top-to-bottom.
fn help_menu(color: bool) -> String {
    // (heading, command, dim, reset) — all empty when color is off, so the menu
    // degrades to clean plain text. ANSI codes are zero-width, so the literal
    // column padding still lines up.
    let (h, c, d, r) = if color {
        ("\x1b[1;36m", "\x1b[1m", "\x1b[2m", "\x1b[0m")
    } else {
        ("", "", "", "")
    };

    // One command row, its description aligned to a shared column.
    let row = |name: &str, args: &str, desc: &str| {
        let visible = name.chars().count() + if args.is_empty() { 0 } else { 1 + args.chars().count() };
        let gap = " ".repeat(20usize.saturating_sub(visible).max(2));
        let args = if args.is_empty() { String::new() } else { format!(" {args}") };
        format!("  {c}{name}{r}{args}{gap}{d}{desc}{r}\n")
    };

    let groups: [(&str, &[(&str, &str, &str)]); 5] = [
        ("Find & describe a symbol", &[
            ("refs", "<symbol>", "Every reference (reads, writes, calls) to a symbol, cross-file."),
            ("callers", "<symbol>", "Every call site of a symbol."),
            ("defs", "<symbol>", "Every definition (function / class / variable / import)."),
            ("describe", "<symbol>", "Signature, callers, callees & reaching tests — one pack."),
        ]),
        ("Graph & relationships", &[
            ("graph", "<symbol>", "Transitive call graph: callees, or callers with --reverse."),
            ("hierarchy", "<class>", "Supertypes, subclasses, and the override map."),
            ("imports", "[module]", "Import graph: edges, reverse deps (--reverse), or --cycles."),
        ]),
        ("Effects, tests & dead code", &[
            ("effects", "<symbol>", "Side effects it transitively performs (fs / net / db / …)."),
            ("tests", "<symbol>", "Which collected tests are call-wired to a symbol."),
            ("deadcode", "", "Callables reachable from no entrypoint (candidates)."),
        ]),
        ("Project surface", &[
            ("inputs", "", "Env vars, files, CLI args & settings the project reads."),
            ("mock-targets", "", "Resolve every mock.patch(...) and flag drifted targets."),
            ("canonical", "", "Most-used helpers, untested public surface, test inventory."),
        ]),
        ("Dynamic", &[
            ("trace", "", "Side effects actually performed at runtime."),
            ("effect-diff", "", "Static effect surface vs. what code really executes."),
            ("change-coverage", "", "Which changed lines the test suite covers (--base <ref>)."),
            ("shapes", "", "Concrete return types each callable produced at runtime."),
        ]),
    ];

    let mut s = String::from("{about}\n\n");
    s += &format!("{d}Usage:{r} {{usage}}\n");
    for (heading, rows) in groups {
        s += &format!("\n{h}{heading}{r}\n");
        for (name, args, desc) in rows {
            s += &row(name, args, desc);
        }
    }
    s += &format!("\n{h}Options{r}\n{{options}}\n");
    s += &format!("\n{d}pyq {} ({}){r}\n", env!("CARGO_PKG_VERSION"), env!("PYQ_GIT_SHA"));
    s += &format!("\n{d}Run `pyq <command> --help` for the full description of any command.{r}");
    s
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
        | Command::Describe { symbol }
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
            let files = cache::index_tree(&cli.root)?;
            query_inputs(&files)
        }
        Command::MockTargets => query_mock_targets(cli)?,
        Command::Hierarchy { symbol } => query_hierarchy(cli, symbol)?,
        Command::Deadcode => query_deadcode(cli)?,
        Command::Canonical => canonical::query(&cli.root)?,
        Command::Imports {
            module,
            reverse,
            cycles,
        } => {
            let files = cache::index_tree(&cli.root)?;
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
        Command::Describe { symbol } => describe::query(&cli.root, symbol)?,
        Command::Tests { symbol } => query_tests(cli, symbol)?,
        Command::Trace {
            pytest_args,
            python,
        } => query_trace(cli, pytest_args, python.as_deref())?,
        Command::EffectDiff {
            pytest_args,
            python,
        } => query_effect_diff(cli, pytest_args, python.as_deref())?,
        Command::ChangeCoverage {
            base,
            pytest_args,
            python,
        } => query_change_cov(cli, base, pytest_args, python.as_deref())?,
        Command::Shapes {
            pytest_args,
            python,
        } => query_shapes(cli, pytest_args, python.as_deref())?,
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
    let files = cache::index_tree(&cli.root)?;
    let scope = walk::walked_py_files(&cli.root);
    let resolver = UnifiedResolver::new(&cli.root, files, scope)?;
    let locs = query(&resolver, symbol)?;
    let results = locs.iter().map(loc_to_json).collect::<Vec<_>>();
    let summary = format!("{} {} of `{}`", results.len(), kind, symbol);
    Ok(Envelope::new(json!({ "kind": kind, "target": symbol }), results).with_summary(summary))
}

fn loc_to_json(loc: &Loc) -> serde_json::Value {
    // Group by kind (the classifier that used to repeat on every line); the body
    // is just the resolved target when the use is ambiguous, else nothing — so a
    // grouped row is a clean column of locations.
    let cols: Vec<String> = match &loc.resolves_to {
        Some(target) => vec![format!("→ {target}")],
        None => Vec::new(),
    };
    let mut v = json!({
        "loc": format!("{}:{}:{}", loc.path, loc.line, loc.col),
        "label": loc.kind,
        "role": loc.role,
        "group": loc.kind,
        "cols": cols,
    });
    if let Some(target) = &loc.resolves_to {
        v["resolves_to"] = json!(target);
    }
    v
}

/// The trailing component of a dotted FQN (`pkg.mod.f` → `f`) — the readable
/// short form for a `via` edge in the human view.
fn leaf(fqn: &str) -> &str {
    fqn.rsplit('.').next().unwrap_or(fqn)
}

/// The transitive call graph of a symbol — forward (callees) or, with
/// `--reverse`, backward (callers) closure over stable fully-qualified IDs.
fn query_graph(
    cli: &Cli,
    symbol: &str,
    reverse: bool,
    depth: Option<usize>,
) -> anyhow::Result<Envelope> {
    let (files, fingerprint) = cache::indexed(&cli.root)?;
    let scope = walk::walked_py_files(&cli.root);
    let graph = cache::call_graph(&cli.root, &files, scope, &fingerprint)?;
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
    let (files, fingerprint) = cache::indexed(&cli.root)?;
    let scope = walk::walked_py_files(&cli.root);
    // The classes pytest collects (Test*-named or *TestCase-subclassing) — built
    // from the index up front, since a graph node alone carries no base-class info.
    let test_classes = tests_map::test_class_fqns(&files);
    let graph = cache::call_graph(&cli.root, &files, scope, &fingerprint)?;
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
                // Sectioned by reachability ring; the body is the test FQN and
                // the edge it arrived on.
                "group": format!("depth {}", n.depth),
                "cols": [n.fqn.clone(), format!("via {}", leaf(&n.via))],
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
        // Concentric rings: one section per hop from the root.
        "group": format!("depth {}", node.depth),
        "cols": [node.fqn.clone(), format!("via {}", leaf(&node.via))],
    })
}

/// The transitive effect surface of a symbol: the side effects performed by the
/// symbol and everything it transitively calls (forward call closure), plus the
/// import-time effects of every module that contributes a reachable callable.
fn query_effects(cli: &Cli, symbol: &str) -> anyhow::Result<Envelope> {
    let (files, fingerprint) = cache::indexed(&cli.root)?;
    let scope = walk::walked_py_files(&cli.root);
    let graph = cache::call_graph(&cli.root, &files, scope, &fingerprint)?;
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
            // Section by category; columns are the API, its owner, and an
            // import-time flag (blank when it runs inside a function).
            let flag = if e.import_time { "import-time" } else { "" };
            rows.push((
                loc.clone(),
                json!({
                    "loc": loc,
                    "label": label,
                    "effect": cat,
                    "api": e.detail,
                    "owner": owner,
                    "import_time": e.import_time,
                    "group": cat,
                    "cols": [e.detail.clone(), owner.clone(), flag.to_string()],
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

/// Effect categories the runtime audit hook can actually observe, so a
/// disagreement with the static surface is meaningful. `env` (writes only),
/// `random`, `clock`, and `global` have no audit event — a static-only finding
/// there is "the dynamic tier can't see it," not "the static tier over-reached."
const AUDITABLE: &[&str] = &["fs", "network", "subprocess", "db"];

/// effect-diff (#9.3): join the static effect surface against what the suite
/// actually performed at runtime, into three buckets —
///   `confirmed`     static predicted it, runtime did it;
///   `dynamic-only`  runtime did it, static missed it (the dynamic-dispatch
///                   blind spot — the finding that makes the dynamic tier worth
///                   running);
///   `static-only`   static predicted it, runtime didn't — over-approximation
///                   or simply not exercised by the suite (distinguishing the
///                   two wants change-coverage, #9.4); in an unaudited category
///                   it is instead `unverifiable`.
/// Both tiers key effects on the same `(owner FQN, category)`, so the join is
/// exact.
fn query_effect_diff(
    cli: &Cli,
    pytest_args: &[String],
    python: Option<&str>,
) -> anyhow::Result<Envelope> {
    // Static side: every effect site across the project, keyed (owner, cat),
    // keeping one representative location for the report.
    let files = cache::index_tree(&cli.root)?;
    let mut static_map: std::collections::BTreeMap<(String, &'static str), String> =
        std::collections::BTreeMap::new();
    for f in &files {
        for e in &f.effects {
            let owner = scope_fqn(&f.path, &e.scope);
            let cat = effect_kind_str(e.kind);
            let loc = format!("{}:{}:{}", f.path, e.pos.line, e.pos.col);
            static_map.entry((owner, cat)).or_insert(loc);
        }
    }

    // Dynamic side: run the suite under the ledger; collect observed (owner,
    // cat), dropping `import` (a load event, not one of the static effect
    // categories).
    let mut opts = TraceOptions::new(cli.root.clone());
    opts.python = python.map(str::to_string).unwrap_or_else(default_python);
    opts.pytest_args = pytest_args.to_vec();
    let observed = observed_effects(&opts)?;
    let pytest_exit = observed.query.get("pytest_exit").cloned();
    let mut dynamic_set: BTreeSet<(String, String)> = BTreeSet::new();
    for r in &observed.results {
        let (owner, cat) = match (r.get("owner").and_then(|v| v.as_str()),
                                  r.get("effect").and_then(|v| v.as_str())) {
            (Some(o), Some(c)) if c != "import" => (o.to_string(), c.to_string()),
            _ => continue,
        };
        dynamic_set.insert((owner, cat));
    }

    let static_keys: BTreeSet<(String, String)> = static_map
        .keys()
        .map(|(o, c)| (o.clone(), c.to_string()))
        .collect();

    let mut results: Vec<serde_json::Value> = Vec::new();
    let (mut n_confirmed, mut n_dynamic, mut n_static, mut n_unverif) = (0, 0, 0, 0);

    // confirmed + static-only/unverifiable, walking the static surface.
    for ((owner, cat), loc) in &static_map {
        let key = (owner.clone(), cat.to_string());
        if dynamic_set.contains(&key) {
            n_confirmed += 1;
            results.push(json!({
                "status": "confirmed", "effect": cat, "owner": owner, "loc": loc,
                "label": format!("confirmed {cat}  {owner}"),
                "group": "confirmed", "cols": [cat.to_string(), owner.clone()],
            }));
        } else if AUDITABLE.contains(cat) {
            n_static += 1;
            results.push(json!({
                "status": "static-only", "effect": cat, "owner": owner, "loc": loc,
                "label": format!("static-only {cat}  {owner} (over-approx or unexercised)"),
                "group": "static-only", "cols": [cat.to_string(), owner.clone(), "over-approx or unexercised".to_string()],
            }));
        } else {
            n_unverif += 1;
            results.push(json!({
                "status": "unverifiable", "effect": cat, "owner": owner, "loc": loc,
                "label": format!("unverifiable {cat}  {owner} (category not audited)"),
                "group": "unverifiable", "cols": [cat.to_string(), owner.clone(), "category not audited".to_string()],
            }));
        }
    }
    // dynamic-only: observed effects the static surface never predicted.
    for (owner, cat) in &dynamic_set {
        if !static_keys.contains(&(owner.clone(), cat.clone())) {
            n_dynamic += 1;
            results.push(json!({
                "status": "dynamic-only", "effect": cat, "owner": owner,
                "label": format!("dynamic-only {cat}  {owner} (static missed this edge)"),
                "group": "dynamic-only", "cols": [cat.clone(), owner.clone(), "static missed this edge".to_string()],
            }));
        }
    }

    // Stable order: status bucket, then category, then owner.
    let bucket_rank = |s: &str| match s {
        "dynamic-only" => 0,
        "confirmed" => 1,
        "static-only" => 2,
        _ => 3,
    };
    results.sort_by(|a, b| {
        let sa = a["status"].as_str().unwrap_or("");
        let sb = b["status"].as_str().unwrap_or("");
        bucket_rank(sa)
            .cmp(&bucket_rank(sb))
            .then(a["effect"].as_str().cmp(&b["effect"].as_str()))
            .then(a["owner"].as_str().cmp(&b["owner"].as_str()))
    });

    let summary = format!(
        "effect-diff: {n_dynamic} dynamic-only, {n_confirmed} confirmed, \
         {n_static} static-only, {n_unverif} unverifiable"
    );
    let mut query = json!({ "kind": "effect-diff" });
    if let (Some(obj), Some(exit)) = (query.as_object_mut(), pytest_exit) {
        obj.insert("pytest_exit".to_string(), exit);
    }

    let mut warnings = vec![
        "dynamic-only effects are the dynamic-dispatch edges the static surface \
         can't see — the reason to run this."
            .to_string(),
        "static-only ≠ over-approximation: the suite may simply not exercise that \
         path. change-coverage (#9.4) will separate the two."
            .to_string(),
    ];
    // Carry through the ledger's own caveats (unaudited categories, dropped
    // non-project hits) so they aren't lost in the join.
    warnings.extend(observed.warnings.iter().cloned());

    Ok(Envelope::new(query, results)
        .with_summary(summary)
        .with_warnings(warnings))
}

/// change-coverage (#9.4): join the lines changed since `base` against per-test
/// runtime line coverage. Each changed line is `covered` (with the tests that
/// executed it) or `uncovered`; a changed file no test reaches at all is called
/// out. On a pre-3.12 interpreter (`sys.monitoring` absent) we report the
/// changed lines with unknown coverage and say so, rather than implying they're
/// all untested.
fn query_change_cov(
    cli: &Cli,
    base: &str,
    pytest_args: &[String],
    python: Option<&str>,
) -> anyhow::Result<Envelope> {
    let changed = change_cov::changed_lines(&cli.root, base)?;

    let mut opts = TraceOptions::new(cli.root.clone());
    opts.python = python.map(str::to_string).unwrap_or_else(default_python);
    opts.pytest_args = pytest_args.to_vec();
    let cov = observed_coverage(&opts)?;

    let total_changed: usize = changed.values().map(|s| s.len()).sum();
    let query = json!({
        "kind": "change-coverage",
        "base": base,
        "python": cov.python,
        "pytest_exit": cov.pytest_exit,
    });

    // Pre-3.12: no line coverage. Report what changed, flag the gap, don't lie.
    if !cov.monitoring_available {
        let results: Vec<serde_json::Value> = changed
            .iter()
            .flat_map(|(file, lines)| {
                lines.iter().map(move |ln| {
                    json!({
                        "loc": format!("{file}:{ln}"),
                        "status": "unknown",
                        "label": format!("unknown {file}:{ln} (line coverage needs Python 3.12+)"),
                        "group": "unknown", "cols": [],
                    })
                })
            })
            .collect();
        return Ok(Envelope::new(query, results)
            .with_summary(format!(
                "{total_changed} changed line(s); coverage unavailable on Python {}",
                cov.python
            ))
            .with_warnings(vec![format!(
                "per-line coverage needs the `sys.monitoring` API (Python 3.12+); \
                 ran under {} — changed lines reported with unknown coverage",
                cov.python
            )]));
    }

    let mut results: Vec<serde_json::Value> = Vec::new();
    let (mut covered, mut uncovered) = (0usize, 0usize);
    let mut files_with_no_cover: Vec<String> = Vec::new();

    for (file, lines) in &changed {
        let mut any_covered = false;
        for &ln in lines {
            if cov.is_covered(file, ln) {
                any_covered = true;
                covered += 1;
                let mut tests = cov.covering_tests(file, ln);
                tests.sort();
                let n_tests = tests.len();
                results.push(json!({
                    "loc": format!("{file}:{ln}"),
                    "status": "covered",
                    "tests": tests,
                    "label": format!("covered {file}:{ln}  ({} test{})",
                        n_tests, if n_tests == 1 { "" } else { "s" }),
                    "group": "covered",
                    "cols": [format!("{n_tests} test{}", if n_tests == 1 { "" } else { "s" })],
                }));
            } else {
                uncovered += 1;
                results.push(json!({
                    "loc": format!("{file}:{ln}"),
                    "status": "uncovered",
                    "label": format!("uncovered {file}:{ln}"),
                    "group": "uncovered", "cols": [],
                }));
            }
        }
        if !any_covered && !lines.is_empty() {
            files_with_no_cover.push(file.clone());
        }
    }

    // uncovered first (the actionable rows), then by location.
    results.sort_by(|a, b| {
        let rank = |s: &str| if s == "uncovered" { 0 } else { 1 };
        rank(a["status"].as_str().unwrap_or(""))
            .cmp(&rank(b["status"].as_str().unwrap_or("")))
            .then(a["loc"].as_str().cmp(&b["loc"].as_str()))
    });

    let summary = format!(
        "change-coverage vs {base}: {covered}/{total_changed} changed lines covered, \
         {uncovered} uncovered across {} file(s)",
        changed.len()
    );
    let mut warnings = Vec::new();
    if !files_with_no_cover.is_empty() {
        warnings.push(format!(
            "{} changed file(s) have no covered changed line: {}",
            files_with_no_cover.len(),
            files_with_no_cover.join(", ")
        ));
    }
    warnings.push(
        "coverage is per static line execution; dynamic dispatch is followed (it's \
         the runtime), but a line not run by *this* suite is 'uncovered', not 'dead'."
            .to_string(),
    );

    Ok(Envelope::new(query, results)
        .with_summary(summary)
        .with_warnings(warnings))
}

/// observed shapes (#9.5): the concrete return types each callable produced at
/// runtime. Runtime evidence next to ty's static inference — a callable that
/// only ever returned `int` while annotated `-> Any`, or no annotation at all,
/// is a candidate to tighten. Needs 3.12+; degrades with a warning otherwise.
fn query_shapes(
    cli: &Cli,
    pytest_args: &[String],
    python: Option<&str>,
) -> anyhow::Result<Envelope> {
    let mut opts = TraceOptions::new(cli.root.clone());
    opts.python = python.map(str::to_string).unwrap_or_else(default_python);
    opts.pytest_args = pytest_args.to_vec();
    let shapes = observed_shapes(&opts)?;

    let query = json!({
        "kind": "shapes",
        "python": shapes.python,
        "pytest_exit": shapes.pytest_exit,
    });

    if !shapes.monitoring_available {
        return Ok(Envelope::new(query, Vec::new())
            .with_summary(format!(
                "observed shapes need Python 3.12+; ran under {}",
                shapes.python
            ))
            .with_warnings(vec![format!(
                "return-type observation needs the `sys.monitoring` API (Python \
                 3.12+); ran under {} — no shapes collected",
                shapes.python
            )]));
    }

    let results: Vec<serde_json::Value> = shapes
        .returns
        .iter()
        .map(|(fqn, types)| {
            json!({
                "owner": fqn,
                "returns": types,
                "label": format!("{fqn} -> {}", types.join(" | ")),
                // No loc (runtime observation); two columns: callable and the
                // union of return types it was seen to produce.
                "cols": [fqn.clone(), format!("-> {}", types.join(" | "))],
            })
        })
        .collect();

    let summary = format!(
        "observed return shapes for {} callable(s)",
        results.len()
    );
    Ok(Envelope::new(query, results).with_summary(summary).with_warnings(vec![
        "runtime evidence from this suite only — a type never seen here may still \
         occur; absence is not proof. Static inference (ty) remains the oracle."
            .to_string(),
    ]))
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
                "group": kind,
                "cols": [i.value.clone()],
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
    let (files, fingerprint) = cache::indexed(&cli.root)?;
    let scope = walk::walked_py_files(&cli.root);
    let graph = cache::call_graph(&cli.root, &files, scope, &fingerprint)?;
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
                "group": "supertypes", "cols": [base.clone()],
            }));
        }
        for ext in h.external_bases(root) {
            results.push(json!({
                "loc": root_loc,
                "label": format!("supertype {ext} (external)"),
                "relation": "supertype-external", "fqn": ext,
                "group": "supertypes (external)", "cols": [ext.clone()],
            }));
        }
        // Subclasses (transitive, first-party).
        for sub in h.subclasses(root) {
            results.push(json!({
                "loc": loc_of.get(&sub).cloned().unwrap_or_else(|| root_loc.clone()),
                "label": format!("subtype {sub}"),
                "relation": "subtype", "fqn": sub,
                "group": "subtypes", "cols": [sub.clone()],
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
                        "group": "overrides", "cols": [format!("{mfqn}  →  {base}.{m}")],
                    }));
                }
                for sub in h.overridden_by(root, m) {
                    results.push(json!({
                        "loc": loc_of.get(&format!("{sub}.{m}")).cloned().unwrap_or_else(|| root_loc.clone()),
                        "label": format!("{sub}.{m} overrides {mfqn}"),
                        "relation": "overridden-by", "fqn": format!("{sub}.{m}"),
                        "group": "overridden by", "cols": [format!("{sub}.{m}  →  {mfqn}")],
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
    let (files, fingerprint) = cache::indexed(&cli.root)?;
    let scope = walk::walked_py_files(&cli.root);
    let graph = cache::call_graph(&cli.root, &files, scope, &fingerprint)?;
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
                "group": d.kind,
                "cols": [d.fqn.clone()],
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
    let files = cache::index_tree(&cli.root)?;
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
            // Section by resolution status; columns are the patched target and
            // (for drifted/unverifiable) the reason.
            let cols: Vec<String> = match &detail {
                Some(why) => vec![shown.to_string(), why.clone()],
                None => vec![shown.to_string()],
            };
            rows.push((
                format!("{tag}{loc}"),
                json!({
                    "loc": loc,
                    "label": label,
                    "target": m.target,
                    "status": tag,
                    "group": tag,
                    "cols": cols,
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
                "cols": [path.join(" → ")],
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
                    rows.push((loc.clone(), json!({ "loc": loc, "label": format!("imported by {}", e.importer), "cols": [e.importer.clone()] })));
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
                    let group = if e.internal { "internal" } else { "external" };
                    rows.push((format!("{}{}", e.target, loc), json!({ "loc": loc, "label": format!("imports {}{}", e.target, tag), "group": group, "cols": [e.target.clone()] })));
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
                // Section per importer module; the body is the edge target.
                rows.push((format!("{}{}", e.importer, loc), json!({ "loc": loc, "label": format!("{} → {}{}", e.importer, e.target, tag), "group": e.importer.clone(), "cols": [format!("→ {}{}", e.target, tag)] })));
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
