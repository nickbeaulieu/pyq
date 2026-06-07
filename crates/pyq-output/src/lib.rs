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
    /// Things the query couldn't do precisely — an over-approximate match, a
    /// blind spot, a skipped path. Surfacing these lets a consumer know when to
    /// fall back to reading the file. Omitted from JSON when empty.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

impl Envelope {
    pub fn new(query: QueryDesc, results: Vec<Value>) -> Self {
        Envelope {
            tool: "pyq",
            count: results.len(),
            summary: String::new(),
            query,
            results,
            warnings: Vec::new(),
        }
    }

    pub fn with_summary(mut self, summary: impl Into<String>) -> Self {
        self.summary = summary.into();
        self
    }

    pub fn with_warnings(mut self, warnings: Vec<String>) -> Self {
        self.warnings = warnings;
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
        for w in &self.warnings {
            out.push_str("! ");
            out.push_str(w);
            out.push('\n');
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
        // A result with no source location (e.g. an effect observed only at
        // runtime) still renders its human label rather than dumping JSON.
        (None, Some(label)) => label.to_string(),
        _ => serde_json::to_string(r).unwrap_or_default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn renders_loc_and_label() {
        let r = json!({"loc": "a.py:1:1", "label": "fs open"});
        assert_eq!(render_result_line(&r), "a.py:1:1  fs open");
    }

    #[test]
    fn renders_label_without_loc() {
        // A runtime-only observation has no source location but still reads as
        // its label, not a JSON dump.
        let r = json!({"label": "dynamic-only subprocess  pkg.ops.f", "owner": "pkg.ops.f"});
        assert_eq!(render_result_line(&r), "dynamic-only subprocess  pkg.ops.f");
    }

    #[test]
    fn falls_back_to_json_without_label() {
        let r = json!({"owner": "pkg.ops.f"});
        assert_eq!(render_result_line(&r), r#"{"owner":"pkg.ops.f"}"#);
    }
}
