//! The shared output envelope for every `pyq` verb.
//!
//! One shape for all queries: `{ tool, query, summary, count, results }`. Two
//! renderers — a token-frugal human view (default, even when piped) and a
//! compact `--json` envelope (opt-in). Keeping this in its own crate is
//! deliberate: the `--baseline` differential machinery (the question an
//! iterating agent actually asks) is generic over result sets and every verb
//! reuses it.

use serde::Serialize;
use serde_json::Value;

/// How to render an [`Envelope`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Channel {
    /// Framed, dim-secondary, one-result-per-line. Default for TTY and pipe.
    Human,
    /// Compact single-line JSON envelope. Opt-in via `--json`.
    Json,
    /// Indented JSON.
    Pretty,
}

/// A structured description of the query that produced these results, e.g.
/// `{ "kind": "refs", "symbol": "User" }`. Structured (not a string) so the
/// envelope never leaks an opaque `query:"..."` blob.
pub type QueryDesc = Value;

/// The universal result envelope.
#[derive(Debug, Serialize)]
pub struct Envelope {
    pub tool: &'static str,
    pub query: QueryDesc,
    pub summary: String,
    pub count: usize,
    pub results: Vec<Value>,
}

impl Envelope {
    pub fn new(query: QueryDesc, results: Vec<Value>) -> Self {
        Envelope {
            tool: "pyq",
            count: results.len(),
            summary: String::new(),
            query,
            results,
        }
    }

    pub fn with_summary(mut self, summary: impl Into<String>) -> Self {
        self.summary = summary.into();
        self
    }

    /// Render to a string for the given channel.
    pub fn render(&self, channel: Channel) -> String {
        match channel {
            Channel::Json => serde_json::to_string(self).unwrap_or_default(),
            Channel::Pretty => serde_json::to_string_pretty(self).unwrap_or_default(),
            Channel::Human => self.render_human(),
        }
    }

    fn render_human(&self) -> String {
        let mut out = String::new();
        if !self.summary.is_empty() {
            out.push_str(&self.summary);
            out.push('\n');
        }
        for r in &self.results {
            out.push_str(&render_result_line(r));
            out.push('\n');
        }
        if out.is_empty() {
            out.push_str("no results\n");
        }
        out
    }
}

/// Best-effort one-line rendering of a result object. Verbs emit a `loc` string
/// (`file:line:col`) plus a `label`; anything else falls back to compact JSON.
fn render_result_line(r: &Value) -> String {
    let loc = r.get("loc").and_then(Value::as_str);
    let label = r.get("label").and_then(Value::as_str);
    match (loc, label) {
        (Some(loc), Some(label)) => format!("{loc}  {label}"),
        (Some(loc), None) => loc.to_string(),
        _ => serde_json::to_string(r).unwrap_or_default(),
    }
}
