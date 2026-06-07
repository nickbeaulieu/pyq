//! The `describe` verb — a symbol's signature + neighbourhood, in one pack.
//!
//! The token-frugal "tell me about X": instead of an agent grepping the def,
//! then its callers, then its callees, then the tests that touch it, `describe`
//! answers all of it in one envelope. The static facets (signature, decorators,
//! docstring, def line-span) come straight off the syntactic index; the
//! neighbourhood is a depth-1 projection of the [`CallGraph`] (#10) — immediate
//! callees (what it reaches in one hop), immediate callers (who reaches it in
//! one hop), and the collected tests that transitively reach it (the same
//! reverse-closure lens the `tests` verb uses).
//!
//! Same boundary as the rest of the graph-backed verbs: reach through dynamic /
//! attribute dispatch is invisible, so an empty caller/callee/test set means "no
//! *static* edge found," not "none exists." When the name is ambiguous (several
//! defs match), the neighbourhood is the union over them — flagged, with one
//! definition row per resolved def so the signatures stay distinct.

use crate::{plural, tests_map, walk};
use pyq_index::{Def, DefKind, FileIndex};
use pyq_output::Envelope;
use pyq_resolve::{scope_fqn, CallGraph, Direction, GraphNode};
use serde_json::{json, Value};
use std::collections::HashMap;

/// Build the description of `symbol` over the project at `root`.
pub fn query(root: &str, symbol: &str) -> anyhow::Result<Envelope> {
    let files = walk::index_tree(root)?;
    let scope = walk::walked_py_files(root);
    // The classes pytest collects, for the reaching-tests filter — built from the
    // index before `files` is cloned into the graph (a graph node carries no
    // base-class info).
    let test_classes = tests_map::test_class_fqns(&files);

    // FQN → its def site, so a resolved root maps back to its signature/doc/span.
    let mut def_by_fqn: HashMap<String, (&FileIndex, &Def)> = HashMap::new();
    for f in &files {
        for d in &f.defs {
            if matches!(d.kind, DefKind::Function | DefKind::Class) {
                let mut s = d.container.clone();
                s.push(d.name.clone());
                def_by_fqn.insert(scope_fqn(&f.path, &s), (f, d));
            }
        }
    }

    let graph = CallGraph::new(root, files.clone(), scope)?;
    // Depth-1 callees; the full reverse closure (immediate callers are its
    // depth-1 nodes, reaching tests its test nodes at any depth) — two walks
    // cover all three neighbourhood facets.
    let fwd = graph.closure(symbol, Direction::Forward, Some(1));
    let rev = graph.closure(symbol, Direction::Reverse, None);
    let roots = &fwd.roots;

    let mut results: Vec<Value> = Vec::new();

    // The definition facet: one row per resolved root.
    for root_fqn in roots {
        if let Some((f, d)) = def_by_fqn.get(root_fqn) {
            results.push(definition_row(root_fqn, f, d));
        }
    }

    // Immediate callers — reverse closure at depth 1.
    let mut callers: Vec<&GraphNode> = rev.nodes.iter().filter(|n| n.depth == 1).collect();
    callers.sort_by(|a, b| a.fqn.cmp(&b.fqn));
    for n in &callers {
        results.push(neighbour_row("caller", n, false));
    }

    // Immediate callees — forward closure (capped at depth 1).
    let mut callees: Vec<&GraphNode> = fwd.nodes.iter().collect();
    callees.sort_by(|a, b| a.fqn.cmp(&b.fqn));
    for n in &callees {
        results.push(neighbour_row("callee", n, false));
    }

    // Reaching tests — reverse closure filtered to collected test nodes; carries
    // depth + via so the call path back to the symbol is visible.
    let mut tests: Vec<&GraphNode> = rev
        .nodes
        .iter()
        .filter(|n| tests_map::is_test_node(n, &test_classes))
        .collect();
    tests.sort_by(|a, b| (a.depth, &a.fqn).cmp(&(b.depth, &b.fqn)));
    for n in &tests {
        results.push(neighbour_row("test", n, true));
    }

    let summary = if roots.is_empty() {
        format!("no function or class named `{symbol}` found")
    } else {
        format!(
            "describe `{symbol}`: {} {}, {} immediate {}, {} {}, {} reaching {}",
            roots.len(),
            plural(roots.len(), "def"),
            callers.len(),
            plural(callers.len(), "caller"),
            callees.len(),
            plural(callees.len(), "callee"),
            tests.len(),
            plural(tests.len(), "test"),
        )
    };

    let query = json!({ "kind": "describe", "target": symbol, "roots": roots });
    let env = Envelope::new(query, results).with_summary(summary);

    let mut warnings = Vec::new();
    if roots.is_empty() {
        warnings.push(format!("no function or class named `{symbol}` found"));
    } else {
        if roots.len() > 1 {
            warnings.push(format!(
                "`{symbol}` is ambiguous — {} defs match; callers/callees/tests are the \
                 union over all of them (qualify the name to disambiguate)",
                roots.len()
            ));
        }
        warnings.push(
            "static over-approximation: callers/callees/tests reachable only via \
             dynamic/attribute dispatch are not shown"
                .to_string(),
        );
    }
    Ok(env.with_warnings(warnings))
}

/// The definition facet: signature/decorators/docstring/span for one resolved
/// root, both as structured fields and a one-line human label.
fn definition_row(fqn: &str, f: &FileIndex, d: &Def) -> Value {
    let loc = format!("{}:{}:{}", f.path, d.pos.line, d.pos.col);
    let kind = if d.kind == DefKind::Class { "class" } else { "def" };
    // A function's signature is its param/return list; a class's is its bases.
    let sig = match d.kind {
        DefKind::Class if !d.bases.is_empty() => format!("({})", d.bases.join(", ")),
        DefKind::Class => String::new(),
        _ => d.signature.clone().unwrap_or_default(),
    };
    let decos: String = d.decorators.iter().map(|x| format!("@{x} ")).collect();
    let mut label = format!("{decos}{kind} {}{sig}  [L{}-{}]", d.name, d.pos.line, d.end_line);
    if let Some(doc) = &d.doc {
        label.push_str("  — ");
        label.push_str(&truncate(doc, 80));
    }
    // Human columns: the signature line, the span, and the docstring (when any).
    let signature_cell = format!("{decos}{kind} {}{sig}", d.name);
    let mut cols = vec![signature_cell, format!("L{}-{}", d.pos.line, d.end_line)];
    if let Some(doc) = &d.doc {
        cols.push(format!("— {}", truncate(doc, 80)));
    }
    json!({
        "loc": loc,
        "label": label,
        "role": "definition",
        "fqn": fqn,
        "node_kind": kind,
        "signature": sig,
        "decorators": d.decorators,
        "doc": d.doc,
        "lines": [d.pos.line, d.end_line],
        "group": "definition",
        "cols": cols,
    })
}

/// A caller/callee/test row from a graph node. Tests show their depth + via
/// (the transitive call path); immediate callers/callees are depth 1 by
/// construction, so they don't repeat it.
fn neighbour_row(role: &str, n: &GraphNode, show_depth: bool) -> Value {
    let loc = format!("{}:{}:{}", n.path, n.line, n.col);
    let label = if show_depth {
        format!("{role} {} (depth {}, via {})", n.fqn, n.depth, n.via)
    } else {
        format!("{role} {}", n.fqn)
    };
    // Section header from the role; the body is the FQN, with the reaching path
    // as a second column for tests (callers/callees are depth-1 by construction).
    let group = match role {
        "caller" => "callers",
        "callee" => "callees",
        "test" => "reaching tests",
        other => other,
    };
    let leaf = n.via.rsplit('.').next().unwrap_or(&n.via);
    let cols: Vec<String> = if show_depth {
        vec![n.fqn.clone(), format!("depth {} · via {}", n.depth, leaf)]
    } else {
        vec![n.fqn.clone()]
    };
    json!({
        "loc": loc,
        "label": label,
        "role": role,
        "fqn": n.fqn,
        "node_kind": n.kind,
        "depth": n.depth,
        "via": n.via,
        "group": group,
        "cols": cols,
    })
}

/// Truncate to `max` characters (an ellipsis marking the cut), char-aware so a
/// multibyte docstring never splits mid-codepoint.
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let t: String = s.chars().take(max).collect();
    format!("{t}…")
}
