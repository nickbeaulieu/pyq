//! `pyq` — a queryable static index for Python codebases.
//!
//! Verb-per-invocation CLI (`pyq <verb> [args] [--json]`), mirroring the design
//! pyq is built around: a token-frugal oracle an agent queries for ground truth
//! about code-as-graph. This is the first slice — symbol/reference queries over
//! a directory of Python files, single-file name resolution.

// The ruff/ty parse + salsa analysis stack allocates heavily across rayon
// threads; mimalloc outperforms the platform default (notably musl's) there.
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

mod cache;
mod canonical;
mod change_cov;
mod channel;
mod deadcode;
mod describe;
mod graph;
mod hierarchy;
mod mock;
mod tests_map;
mod upgrade;
mod walk;

use clap::{CommandFactory, FromArgMatches, Parser, Subcommand};
use pyq_dynamic::{default_python, observed_effects, TraceOptions};
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
    // Version carries the channel, build date, and short commit sha (captured
    // in build.rs), so `pyq --version` pins exactly which build is running and
    // `pyq upgrade` knows what it's comparing against.
    version = concat!(
        env!("CARGO_PKG_VERSION"),
        " (", env!("PYQ_CHANNEL"), " ", env!("PYQ_BUILD_DATE"), " ", env!("PYQ_GIT_SHA"), ")"
    ),
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
    /// The transitive effect surface — which side effects (files, network,
    /// subprocess, env, db, randomness, clock, global mutation) a symbol and
    /// everything it transitively calls touch (omit the symbol for the whole
    /// project). Each row is labelled by confidence — `confirmed` (the suite
    /// performed it), `predicted` (static says so, unverified), `observed` (the
    /// run did it but static missed the edge), `unverifiable` (audit-blind
    /// category). Runs your test suite on a cache miss to verify (set
    /// `PYQ_NO_SUITE` to skip). "Is this pure / what does it really touch."
    Effects { symbol: Option<String> },
    /// Which tests reach code. With a symbol: the static test↔code map — the
    /// collected tests whose reverse call graph reaches it, each with the call
    /// path (`via`) and `depth`. With `--base <ref>` (no symbol): the runtime
    /// oracle — which lines changed since that ref the suite actually covers, and
    /// by which tests (`sys.monitoring`, Python 3.12+; degrades on older). The
    /// `--base` form runs your tests.
    Tests {
        /// The symbol whose reaching tests to map. Omit when using `--base`.
        symbol: Option<String>,
        /// Report changed-line coverage since this git ref instead of a symbol's
        /// reaching tests (the absorbed `change-coverage`).
        #[arg(long)]
        base: Option<String>,
    },
    /// One compact context pack for a symbol — signature, decorators, docstring
    /// line, and def line-span, plus its immediate callers, immediate callees,
    /// and the collected tests that reach it. The token-frugal "tell me about
    /// X": everything an agent would otherwise grep for, in one envelope.
    /// Accepts a bare name, a qualified name, or a full FQN.
    Describe { symbol: String },
    /// The external input surface — env vars, config reads, literal files
    /// opened, CLI args, and pydantic settings fields. "What does this need to
    /// run." With no argument, the **app** surface (config the running service
    /// reads), with per-script inputs (management commands, `scripts/`) held
    /// back behind a hint. Pass a script name or path (`inputs backfill_calls`)
    /// to see that script's own inputs.
    Inputs { target: Option<String> },
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
    /// Internal: the raw runtime effect ledger (what the suite actually did),
    /// without the static join. Hidden — `effects` surfaces this fused and
    /// labelled; `trace` stays for debugging the dynamic tier. Runs your tests.
    #[command(hide = true)]
    Trace {
        /// Arguments passed through to pytest (test paths, `-k`, `-m`, …).
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        pytest_args: Vec<String>,
        /// Python interpreter to drive (defaults to `$PYQ_PYTHON` or `python3`).
        #[arg(long)]
        python: Option<String>,
    },
    /// Build the analysis cache for this repo up front — parse every file and
    /// record the full call graph now, so later verbs replay from `~/.pyq`
    /// without constructing ty. The "pay the first-run cost on demand" control.
    /// `index clean` removes this repo's cached index entirely.
    Index {
        #[command(subcommand)]
        action: Option<IndexAction>,
    },
    /// Show or switch the release channel `pyq upgrade` follows. With no
    /// argument, report the configured channel and this build's identity; with
    /// `stable` or `canary`, switch and persist the choice to `~/.pyq/channel`.
    /// Stable tracks tagged releases; canary tracks `main`. Switching only
    /// records intent — run `pyq upgrade` to actually move.
    Channel {
        /// `stable` or `canary`. Omit to show the current channel.
        channel: Option<String>,
    },
    /// Upgrade `pyq` in place to the latest build on the configured channel,
    /// verifying its checksum before replacing the running binary. `--check`
    /// reports what an upgrade would do without installing; `--force`
    /// reinstalls even when already current.
    Upgrade {
        /// Report the available version without installing.
        #[arg(long)]
        check: bool,
        /// Reinstall even when already up to date.
        #[arg(long)]
        force: bool,
    },
}

#[derive(Subcommand)]
enum IndexAction {
    /// Remove this repo's cached index from `~/.pyq` (forces a fresh build next time).
    Clean,
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

    // One menu group: a heading and its `(name, args, desc)` command rows.
    type Group = (&'static str, &'static [(&'static str, &'static str, &'static str)]);
    let groups: [Group; 5] = [
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
            ("inputs", "[script]", "App env/config & settings; a script name for its own inputs."),
            ("mock-targets", "", "Resolve every mock.patch(...) and flag drifted targets."),
            ("canonical", "", "Most-used helpers, untested public surface, test inventory."),
            ("index", "", "Prewarm the ~/.pyq cache (index clean wipes it)."),
        ]),
        ("Distribution", &[
            ("channel", "[name]", "Show or switch the release channel (stable / canary)."),
            ("upgrade", "", "Update pyq in place to the latest build on the channel."),
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
    s += &format!(
        "\n{d}pyq {} ({} {} {}){r}\n",
        env!("CARGO_PKG_VERSION"),
        env!("PYQ_CHANNEL"),
        env!("PYQ_BUILD_DATE"),
        env!("PYQ_GIT_SHA")
    );
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
        | Command::Hierarchy { symbol }
        | Command::Describe { symbol } => Some(symbol.as_str()),
        _ => None,
    };
    if matches!(symbol, Some(s) if s.trim().is_empty()) {
        anyhow::bail!("symbol must not be empty");
    }

    // One query path. `inputs`/`imports` are pure syntactic facts; for
    // `refs`/`callers`/`defs` the Resolver trait merges ty (authoritative,
    // cross-file) with the syntactic scan (ty's blind spots) into one answer.
    let mut envelope = match &cli.command {
        Command::Inputs { target } => {
            let files = cache::index_tree(&cli.root)?;
            query_inputs(&files, target.as_deref())
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
        Command::Callers { symbol } => query_callers(cli, symbol)?,
        Command::Defs { symbol } => resolve(cli, symbol, "defs", |r, s| r.definitions(s))?,
        Command::Graph {
            symbol,
            reverse,
            depth,
        } => query_graph(cli, symbol, *reverse, *depth)?,
        Command::Effects { symbol } => query_effects(cli, symbol.as_deref())?,
        Command::Describe { symbol } => describe::query(&cli.root, symbol)?,
        Command::Tests { symbol, base } => match (base, symbol) {
            // `--base` is the absorbed change-coverage (runtime line coverage of
            // the diff); a bare symbol is the static reaching-tests map.
            (Some(base), _) => query_change_cov(cli, base)?,
            (None, Some(symbol)) => query_tests(cli, symbol)?,
            (None, None) => anyhow::bail!(
                "`tests` needs a symbol (reaching tests) or `--base <ref>` (changed-line coverage)"
            ),
        },
        Command::Trace {
            pytest_args,
            python,
        } => query_trace(cli, pytest_args, python.as_deref())?,
        Command::Index { action } => match action {
            Some(IndexAction::Clean) => query_index_clean(cli)?,
            None => query_index(cli)?,
        },
        Command::Channel { channel } => channel::query(channel.as_deref())?,
        Command::Upgrade { check, force } => upgrade::run(*check, *force)?,
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

/// `callers` — every call site of a symbol, from the ty-backed resolver, with
/// the same framework-dispatch caveat as `tests`/`graph --reverse`: a method a
/// framework drives (a view, a signal handler) can show zero direct callers
/// while being called constantly, so a `0` here must say why. The resolver
/// gives the precise call sites; the call graph + hierarchy supply the caveat.
fn query_callers(cli: &Cli, symbol: &str) -> anyhow::Result<Envelope> {
    let (files, fingerprint) = cache::indexed(&cli.root)?;
    let scope = walk::walked_py_files(&cli.root);
    let graph = cache::call_graph(&cli.root, &files, scope.clone(), &fingerprint)?;
    // Resolve the symbol to its durable FQN(s) without walking (depth 0), then
    // ask whether those are framework-driven.
    let roots = graph.closure(symbol, Direction::Reverse, Some(0)).roots;
    let caveat = if roots.is_empty() {
        None
    } else {
        let hier = hierarchy::Hierarchy::build(&files, &graph);
        deadcode::dispatch_caveat(&roots, &files, &graph, &hier, &cli.root)
    };

    let resolver = UnifiedResolver::new(&cli.root, files, scope)?;
    let locs = resolver.callers(symbol)?;
    let results = locs.iter().map(loc_to_json).collect::<Vec<_>>();
    let summary = format!("{} {} of `{symbol}`", results.len(), plural(results.len(), "caller"));
    let exhaustive = caveat.is_none();
    let query = json!({
        "kind": "callers",
        "target": symbol,
        "exhaustive": exhaustive,
        "caveat": caveat.as_ref().map(|_| "framework-dispatch"),
    });
    let mut envelope = Envelope::new(query, results).with_summary(summary);
    if let Some((fqn, kind)) = &caveat {
        envelope = envelope.with_warnings(vec![format!(
            "`{}` is framework-dispatched ({}) — callers that reach it only through that dispatch aren't shown, so a 0 here is not \"uncalled\".",
            leaf(fqn),
            kind.reason()
        )]);
    }
    Ok(envelope)
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
        format!("{n} {} statically reach `{symbol}`", plural(n, "node"))
    } else {
        format!("{n} {} reachable from `{symbol}`", plural(n, "node"))
    };

    // The forward closure reads each def's own body, so it's exhaustive; the
    // reverse closure misses callers reached through dynamic dispatch — caveat
    // it, but only when the symbol is framework-driven (so the note is evidence,
    // not boilerplate).
    let caveat = if reverse && !closure.roots.is_empty() {
        let hier = hierarchy::Hierarchy::build(&files, &graph);
        deadcode::dispatch_caveat(&closure.roots, &files, &graph, &hier, &cli.root)
    } else {
        None
    };
    let exhaustive = !reverse || (!closure.roots.is_empty() && caveat.is_none());

    // Echo the resolved FQN roots: the durable handle(s) the symbol mapped to,
    // re-queryable after edits even when line numbers move.
    let query = json!({
        "kind": "graph",
        "mode": if reverse { "reverse" } else { "forward" },
        "target": symbol,
        "roots": closure.roots,
        "exhaustive": exhaustive,
        "caveat": caveat.as_ref().map(|_| "framework-dispatch"),
    });
    let mut envelope = Envelope::new(query, results).with_summary(summary);
    // No root means the symbol named no function or class — a 0-result graph
    // that must not read as "found, but isolated."
    if closure.roots.is_empty() {
        envelope = envelope
            .with_warnings(vec![format!("no function or class named `{symbol}` found")]);
    } else if let Some((fqn, kind)) = &caveat {
        envelope = envelope.with_warnings(vec![format!(
            "`{}` is framework-dispatched ({}) — callers that reach it only through that dispatch aren't followed, so a 0 here is not \"unreached\".",
            leaf(fqn),
            kind.reason()
        )]);
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

    // Fire the dynamic-dispatch caveat only when there's evidence it applies to
    // *this* symbol (it's framework-driven), so a `0` from a Django view or a
    // Celery task says why — instead of a blanket disclaimer on every query.
    let caveat = if closure.roots.is_empty() {
        None
    } else {
        let hier = hierarchy::Hierarchy::build(&files, &graph);
        deadcode::dispatch_caveat(&closure.roots, &files, &graph, &hier, &cli.root)
    };
    let exhaustive = !closure.roots.is_empty() && caveat.is_none();
    let query = json!({
        "kind": "tests",
        "target": symbol,
        "roots": closure.roots,
        "exhaustive": exhaustive,
        "caveat": caveat.as_ref().map(|_| "framework-dispatch"),
    });
    let mut envelope = Envelope::new(query, results).with_summary(summary);

    if closure.roots.is_empty() {
        envelope = envelope
            .with_warnings(vec![format!("no function or class named `{symbol}` found")]);
    } else if let Some((fqn, kind)) = &caveat {
        envelope = envelope.with_warnings(vec![format!(
            "`{}` is framework-dispatched ({}) — tests that reach it only through that dispatch aren't followed, so a 0 here is not \"untested\". Confirm runtime coverage with `pyq tests --base`.",
            leaf(fqn),
            kind.reason()
        )]);
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
        // Concentric rings: one section per hop from the root.
        "group": format!("depth {}", node.depth),
        "cols": [node.fqn.clone(), format!("via {}", leaf(&node.via))],
    })
}

/// The transitive effect surface of a symbol: the side effects performed by the
/// symbol and everything it transitively calls (forward call closure), plus the
/// import-time effects of every module that contributes a reachable callable.
/// The transitive effect surface of a symbol (or the whole project when no
/// symbol is given), **fused with the runtime ledger** so every row is labelled
/// by how sure we are — the absorbed `effect-diff`:
///   `confirmed`     static predicted it and the suite performed it;
///   `predicted`     static says so, the run didn't exercise it (over-approx or
///                   uncovered) — or the suite couldn't run at all;
///   `observed`      the run did it but static missed the edge (dynamic dispatch
///                   — the reason to run the suite); has no static loc;
///   `unverifiable`  an effect category the audit hook can't watch
///                   (env-read/random/clock/global).
/// The suite is run on a ledger cache miss (set `PYQ_NO_SUITE` to skip it).
fn query_effects(cli: &Cli, symbol: Option<&str>) -> anyhow::Result<Envelope> {
    let (files, fingerprint) = cache::indexed(&cli.root)?;

    // --- Static effect surface (per site) ---
    // Scoped to a symbol's forward closure, or project-wide with no symbol.
    let scoped = symbol.is_some();
    let mut roots_empty = false;
    let mut reachable: HashSet<String> = HashSet::new();
    if let Some(sym) = symbol {
        let scope = walk::walked_py_files(&cli.root);
        let graph = cache::call_graph(&cli.root, &files, scope, &fingerprint)?;
        let closure = graph.closure(sym, Direction::Forward, None);
        roots_empty = closure.roots.is_empty();
        reachable = closure
            .roots
            .iter()
            .cloned()
            .chain(closure.nodes.iter().map(|n| n.fqn.clone()))
            .collect();
    }
    // A file is "in play" if it defines a reachable callable — importing it runs
    // its import-time effects. (Project-wide: every file is in play.)
    let in_play: HashSet<&str> = files
        .iter()
        .filter(|f| {
            !scoped
                || f.defs.iter().any(|d| {
                    matches!(d.kind, pyq_index::DefKind::Function | pyq_index::DefKind::Class)
                        && reachable.contains(&owner_fqn(&f.path, &d.container, &d.name))
                })
        })
        .map(|f| f.path.as_str())
        .collect();

    struct Site {
        loc: String,
        path: String,
        owner: String,
        cat: &'static str,
        api: String,
        import_time: bool,
    }
    let mut sites: Vec<Site> = Vec::new();
    for f in &files {
        for e in &f.effects {
            let owner = scope_fqn(&f.path, &e.scope);
            let keep = if !scoped {
                true
            } else if e.import_time {
                in_play.contains(f.path.as_str())
            } else {
                reachable.contains(&owner)
            };
            if keep {
                sites.push(Site {
                    loc: format!("{}:{}:{}", f.path, e.pos.line, e.pos.col),
                    path: f.path.clone(),
                    owner,
                    cat: effect_kind_str(e.kind),
                    api: e.detail.clone(),
                    import_time: e.import_time,
                });
            }
        }
    }

    // --- Runtime ledger (cached, or the suite run on demand) ---
    let observed = cache::ledger(&cli.root, &fingerprint, &default_python());
    let static_keys: HashSet<(String, String)> =
        sites.iter().map(|s| (s.owner.clone(), s.cat.to_string())).collect();
    let confidence = |owner: &str, cat: &str| -> &'static str {
        if observed.available && observed.effects.contains(&(owner.to_string(), cat.to_string())) {
            "confirmed"
        } else if AUDITABLE.contains(&cat) {
            "predicted"
        } else {
            "unverifiable"
        }
    };
    // Display order: certain first, then the dynamic-only payoff, then the
    // unverified static predictions, then the audit-blind categories.
    let rank = |c: &str| match c {
        "confirmed" => 0u8,
        "observed" => 1,
        "predicted" => 2,
        _ => 3,
    };

    let mut entries: Vec<(u8, String, String, serde_json::Value)> = Vec::new();
    let mut categories: BTreeSet<String> = BTreeSet::new();
    let mut counts: std::collections::BTreeMap<&'static str, usize> =
        std::collections::BTreeMap::new();
    let mut seen: HashSet<(String, String)> = HashSet::new(); // (loc, api)

    for s in &sites {
        if !seen.insert((s.loc.clone(), s.api.clone())) {
            continue;
        }
        categories.insert(s.cat.to_string());
        let conf = confidence(&s.owner, s.cat);
        *counts.entry(conf).or_default() += 1;
        let in_label = if s.import_time {
            format!("{} (import-time)", module_label(&s.path))
        } else {
            s.owner.clone()
        };
        let flag = if s.import_time { "import-time" } else { "" };
        entries.push((
            rank(conf),
            s.cat.to_string(),
            s.loc.clone(),
            json!({
                "loc": s.loc,
                "label": format!("{conf} {} {}  in {in_label}", s.cat, s.api),
                "confidence": conf,
                "effect": s.cat,
                "api": s.api,
                "owner": s.owner,
                "import_time": s.import_time,
                "group": conf,
                "cols": [s.cat.to_string(), s.api.clone(), s.owner.clone(), flag.to_string()],
            }),
        ));
    }

    // Observed-only: effects the runtime performed that the static surface never
    // predicted — the dynamic-dispatch edges. Scoped queries keep only owners the
    // static closure already reaches (a sound attribution without dynamic call
    // edges); project-wide keeps them all.
    if observed.available {
        let mut dyn_only: Vec<&(String, String)> = observed
            .effects
            .iter()
            .filter(|(owner, cat)| {
                !static_keys.contains(&(owner.clone(), cat.clone()))
                    && (!scoped || reachable.contains(owner))
            })
            .collect();
        dyn_only.sort();
        for (owner, cat) in dyn_only {
            categories.insert(cat.clone());
            *counts.entry("observed").or_default() += 1;
            entries.push((
                rank("observed"),
                cat.clone(),
                String::new(),
                json!({
                    "label": format!("observed {cat}  in {owner} (static missed this edge)"),
                    "confidence": "observed",
                    "effect": cat,
                    "owner": owner,
                    "import_time": false,
                    "group": "observed",
                    "cols": [cat.clone(), String::new(), owner.clone(), String::new()],
                }),
            ));
        }
    }

    entries.sort_by(|a, b| (a.0, &a.1, &a.2).cmp(&(b.0, &b.1, &b.2)));
    let results: Vec<serde_json::Value> = entries.into_iter().map(|e| e.3).collect();
    let cats: Vec<String> = categories.into_iter().collect();

    let target_label = symbol.map(|s| format!(" of `{s}`")).unwrap_or_default();
    let summary = if roots_empty {
        format!("no function or class named `{}` found", symbol.unwrap_or(""))
    } else if results.is_empty() {
        match symbol {
            Some(s) => format!(
                "`{s}` is pure — no static effects across {} reachable {}",
                reachable.len(),
                plural(reachable.len(), "callable")
            ),
            None => "no effects found".to_string(),
        }
    } else {
        let parts: Vec<String> = ["confirmed", "observed", "predicted", "unverifiable"]
            .iter()
            .filter_map(|c| counts.get(c).map(|n| format!("{n} {c}")))
            .collect();
        format!("effects{target_label}: {}", parts.join(", "))
    };

    let query = json!({ "kind": "effects", "target": symbol, "categories": cats });
    let mut warnings: Vec<String> = Vec::new();
    if roots_empty {
        warnings.push(format!(
            "no function or class named `{}` found",
            symbol.unwrap_or("")
        ));
    } else {
        // The honesty footer, now scoped to what the labels can't settle: a
        // `predicted` row may be over-approximation or merely unexercised, and
        // `unverifiable` categories are audit-blind. `confirmed` means the suite
        // performed it; `observed` is an edge the static surface can't see.
        if !observed.available {
            warnings.extend(observed.warnings.iter().cloned());
        } else if !matches!(observed.pytest_exit, None | Some(0) | Some(5)) {
            // Non-zero (and not pytest's "no tests collected" 5): the run was
            // partial, so absence of a `confirmed` label is less conclusive.
            warnings.push(format!(
                "the test suite exited non-zero (code {}) — the runtime ledger may be incomplete",
                observed.pytest_exit.unwrap_or_default()
            ));
        }
        warnings.push(
            "static over-approximation: a `predicted` effect may be unexercised or behind \
             dynamic dispatch; only `confirmed` is proven to run"
                .to_string(),
        );
    }
    Ok(Envelope::new(query, results)
        .with_summary(summary)
        .with_warnings(warnings))
}

/// Effect categories the runtime audit hook can actually observe, so a
/// disagreement with the static surface is meaningful. `env` (writes only),
/// `random`, `clock`, and `global` have no audit event — a static-only finding
/// there is "the dynamic tier can't see it," not "the static tier over-reached."
const AUDITABLE: &[&str] = &["fs", "network", "subprocess", "db"];

/// change-coverage (#9.4): join the lines changed since `base` against per-test
/// runtime line coverage. Each changed line is `covered` (with the tests that
/// executed it) or `uncovered`; a changed file no test reaches at all is called
/// out. On a pre-3.12 interpreter (`sys.monitoring` absent) we report the
/// changed lines with unknown coverage and say so, rather than implying they're
/// all untested.
fn query_change_cov(cli: &Cli, base: &str) -> anyhow::Result<Envelope> {
    let changed = change_cov::changed_lines(&cli.root, base)?;

    // Per-test line coverage comes from the shared runtime ledger — one suite run
    // for effects/shapes/coverage, run on a cache miss (`PYQ_NO_SUITE` skips).
    let (_files, fingerprint) = cache::indexed(&cli.root)?;
    let led = cache::ledger(&cli.root, &fingerprint, &default_python());
    let cov = &led.coverage;

    let total_changed: usize = changed.values().map(|s| s.len()).sum();
    let query = json!({
        "kind": "change-coverage",
        "base": base,
        "python": cov.python,
        "pytest_exit": cov.pytest_exit,
    });

    // No coverage — either the suite couldn't run (no pytest / `PYQ_NO_SUITE`) or
    // the interpreter is pre-3.12. Report what changed, flag the real reason,
    // don't imply the lines are untested.
    if !cov.monitoring_available {
        let results: Vec<serde_json::Value> = changed
            .iter()
            .flat_map(|(file, lines)| {
                lines.iter().map(move |ln| {
                    json!({
                        "loc": format!("{file}:{ln}"),
                        "status": "unknown",
                        "label": format!("unknown {file}:{ln} (no line coverage)"),
                        "group": "unknown", "cols": [],
                    })
                })
            })
            .collect();
        // The suite ran but is pre-3.12 → name the version; it didn't run at all
        // → carry the ledger's own reason (no pytest / skipped).
        let warning = if led.available {
            format!(
                "per-line coverage needs the `sys.monitoring` API (Python 3.12+); \
                 ran under {} — changed lines reported with unknown coverage",
                cov.python
            )
        } else {
            led.warnings.join("; ")
        };
        return Ok(Envelope::new(query, results)
            .with_summary(format!(
                "{total_changed} changed line(s); coverage unavailable{}",
                if led.available { format!(" on Python {}", cov.python) } else { String::new() }
            ))
            .with_warnings(vec![warning]));
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

/// The external input surface (syntactic). With no `target`, the **app**
/// surface — inputs from files that are neither runnable scripts nor tests —
/// plus a hint pointing at the scripts that carry their own inputs. With a
/// `target`, the inputs of the script(s)/file(s) it names (by command name,
/// basename, or path suffix).
fn query_inputs(files: &[FileIndex], target: Option<&str>) -> Envelope {
    match target {
        Some(t) => {
            let matched: Vec<&FileIndex> =
                files.iter().filter(|f| file_matches(&f.path, t)).collect();
            let mut results = Vec::new();
            for f in &matched {
                input_rows(f, &mut results);
            }
            let summary = match matched.as_slice() {
                [] => format!("no file matches `{t}`"),
                [f] => format!("{} inputs in {}", results.len(), f.path),
                _ => {
                    format!("{} inputs across {} files matching `{t}`", results.len(), matched.len())
                }
            };
            Envelope::new(json!({ "kind": "inputs", "target": t }), results).with_summary(summary)
        }
        None => {
            let mut results = Vec::new();
            let mut scripts: Vec<String> = Vec::new();
            for f in files {
                if deadcode::is_script_file(&f.path, f.has_main_guard) {
                    // A script's inputs are its own — surfaced only when queried.
                    if !f.inputs.is_empty() {
                        scripts.push(script_name(&f.path));
                    }
                    continue;
                }
                if tests_map::is_test_file(&f.path) {
                    continue;
                }
                input_rows(f, &mut results);
            }
            let summary = format!("{} app inputs", results.len());
            let mut env = Envelope::new(json!({ "kind": "inputs", "target": null }), results)
                .with_summary(summary);
            if !scripts.is_empty() {
                scripts.sort();
                scripts.dedup();
                let n = scripts.len();
                let shown = scripts.iter().take(6).cloned().collect::<Vec<_>>().join(", ");
                let more = if n > 6 { format!(", … (+{} more)", n - 6) } else { String::new() };
                let (possessive, verb) =
                    if n == 1 { ("", "has its") } else { ("s", "have their") };
                env = env.with_notes(vec![
                    format!(
                        "{n} script{possessive} {verb} own inputs (management commands, scripts/) — query one by name:",
                    ),
                    format!("pyq inputs <name> — e.g. {shown}{more}"),
                ]);
            }
            env
        }
    }
}

/// Push one row per input of `f` into `out` (env/file/arg/setting sections).
fn input_rows(f: &FileIndex, out: &mut Vec<serde_json::Value>) {
    for i in &f.inputs {
        let kind = match i.kind {
            InputKind::Env => "env",
            InputKind::File => "file",
            InputKind::Arg => "arg",
            InputKind::Setting => "setting",
        };
        out.push(json!({
            "loc": format!("{}:{}:{}", f.path, i.pos.line, i.pos.col),
            "label": format!("{kind} {}", i.value),
            "group": kind,
            "cols": [i.value.clone()],
        }));
    }
}

/// A script's queryable name — its file stem (`…/commands/backfill_calls.py` →
/// `backfill_calls`), the token a Django management command is invoked by.
fn script_name(path: &str) -> String {
    std::path::Path::new(path)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(path)
        .to_string()
}

/// Whether `path` is named by `target`: exact path, basename, command name
/// (file stem), or a segment-aligned path suffix (`a/b.py` matches `…/a/b.py`).
fn file_matches(path: &str, target: &str) -> bool {
    let p = std::path::Path::new(path);
    let base = p.file_name().and_then(|s| s.to_str()).unwrap_or(path);
    let stem = p.file_stem().and_then(|s| s.to_str()).unwrap_or(path);
    path == target
        || base == target
        || stem == target
        || (target.contains('/') && path.ends_with(&format!("/{target}")))
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

/// Prewarm the analysis cache: parse every file and record the full call graph
/// now, so subsequent verbs replay from `~/.pyq` without constructing ty. This
/// pays the "first run" cost on demand (the cold record is the expensive part);
/// it's idempotent — a re-run with an unchanged tree replays instead of
/// rebuilding. Reports how much it indexed and where.
/// A two-phase progress bar for `pyq index`'s call-graph record. The phases run
/// **sequentially** — `Edges` (walk every callable) fully completes before
/// `Resolve` (resolve occurrences) — so a single reused bar shows them in turn:
/// no second [`indicatif::ProgressBar`] competing for the terminal's live line
/// (two standalone bars clobber each other; one leaves the other orphaned at 0).
/// Hidden unless drawing to a terminal, so piped / `--json` output stays clean.
struct IndexProgress {
    bar: indicatif::ProgressBar,
    edges_total: u64,
    resolve_total: u64,
    on_resolve: std::sync::atomic::AtomicBool,
}

impl IndexProgress {
    fn new(edges_total: u64, resolve_total: u64, want: bool) -> Self {
        let bar = if want && std::io::stderr().is_terminal() {
            indicatif::ProgressBar::new(edges_total)
        } else {
            indicatif::ProgressBar::hidden()
        };
        bar.set_style(
            indicatif::ProgressStyle::with_template("  {msg:<11} [{bar:30}] {pos}/{len}")
                .unwrap_or_else(|_| indicatif::ProgressStyle::default_bar())
                .progress_chars("=> "),
        );
        bar.set_message("call graph");
        IndexProgress {
            bar,
            edges_total,
            resolve_total,
            on_resolve: std::sync::atomic::AtomicBool::new(false),
        }
    }

    fn tick(&self, phase: pyq_resolve::RecordPhase, count: u64) {
        match phase {
            pyq_resolve::RecordPhase::Edges => {
                // The worklist can discover a few nodes past the def-anchor count;
                // grow the bar rather than pin at 100%.
                if count > self.bar.length().unwrap_or(0) {
                    self.bar.set_length(count);
                }
                self.bar.set_position(count);
            }
            pyq_resolve::RecordPhase::Resolve => {
                // First resolve tick: finish the edge phase at full, then reuse the
                // same line for the resolve phase (new label, length, position).
                if !self.on_resolve.swap(true, std::sync::atomic::Ordering::Relaxed) {
                    self.bar.set_position(self.bar.length().unwrap_or(self.edges_total));
                    self.bar.set_message("resolving");
                    self.bar.set_length(self.resolve_total);
                }
                self.bar.set_position(count);
            }
        }
    }

    fn finish(&self) {
        if let Some(len) = self.bar.length() {
            self.bar.set_position(len);
        }
        self.bar.finish();
    }
}

fn query_index(cli: &Cli) -> anyhow::Result<Envelope> {
    let (files, fingerprint) = cache::indexed(&cli.root)?;
    let n = files.len();
    let scope = walk::walked_py_files(&cli.root);

    // A two-phase progress bar for the call-graph record — the slow part on a big
    // tree. Totals come straight from the parsed facts (def anchors to walk for
    // edges; occurrences to resolve). Drawn only on a TTY; silent when piped or
    // under `--json`, so machine consumers see clean output.
    let edges_total = files
        .iter()
        .flat_map(|f| f.defs.iter())
        .filter(|d| matches!(d.kind, pyq_index::DefKind::Function | pyq_index::DefKind::Class))
        .count() as u64;
    let resolve_total = files
        .iter()
        .map(|f| {
            (f.refs.len() + f.defs.iter().filter(|d| d.kind == pyq_index::DefKind::Import).count())
                as u64
        })
        .sum::<u64>()
        .max(1);
    let progress = IndexProgress::new(edges_total, resolve_total, !cli.json && !cli.pretty);
    cache::call_graph_with_progress(&cli.root, &files, scope, &fingerprint, &|phase, count| {
        progress.tick(phase, count as u64);
    })?;
    progress.finish();

    // An empty fingerprint means caching is unavailable (`PYQ_NO_CACHE`, or no
    // resolvable home) — say so rather than implying we persisted anything.
    if fingerprint.is_empty() {
        return Ok(Envelope::new(
            json!({ "kind": "index", "files": n, "cached": false }),
            vec![],
        )
        .with_summary(format!("parsed {} (caching disabled — nothing persisted)", plural_files(n)))
        .with_warnings(vec![
            "no cache was written (PYQ_NO_CACHE is set, or no home directory could be resolved)"
                .to_string(),
        ]));
    }

    let loc = cache::location(&cli.root)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| "<unavailable>".to_string());
    Ok(Envelope::new(
        json!({ "kind": "index", "files": n, "cached": true, "cache": loc }),
        vec![],
    )
    .with_summary(format!(
        "indexed {}; call graph recorded → {loc}",
        plural_files(n)
    )))
}

/// Remove this repo's cached index. Idempotent — reports plainly when there was
/// nothing to remove.
fn query_index_clean(cli: &Cli) -> anyhow::Result<Envelope> {
    let removed = cache::clean(&cli.root)?;
    Ok(match removed {
        Some(path) => {
            let p = path.to_string_lossy().into_owned();
            Envelope::new(json!({ "kind": "index-clean", "removed": p.clone() }), vec![])
                .with_summary(format!("removed cached index → {p}"))
        }
        None => Envelope::new(json!({ "kind": "index-clean", "removed": null }), vec![])
            .with_summary("no cached index to remove".to_string()),
    })
}

fn plural_files(n: usize) -> String {
    format!("{n} {}", plural(n, "file"))
}

/// Re-export for the walk module.
pub(crate) fn extract_file(path: &str, source: &str) -> FileIndex {
    extract(path, source)
}
