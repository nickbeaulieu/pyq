//! The shared output envelope for every `pyq` verb.
//!
//! One shape for all queries: `{ tool, query, summary, count, results }`. Two
//! renderers — a human view (default, even when piped) that groups results into
//! aligned, blank-line-separated sections with a trailing `notes` block, and a
//! compact `--json` envelope (opt-in). Keeping this in its own crate is
//! deliberate: the `--baseline` differential machinery (the question an
//! iterating agent actually asks) is generic over result sets and every verb
//! reuses it.

use serde::Serialize;
use serde_json::Value;

/// How to render an [`Envelope`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Channel {
    /// Summary header, then results in aligned, blank-line-separated sections,
    /// then a `notes` block for warnings. Default for TTY and pipe.
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
    /// Informational pointers — not problems, just things worth surfacing (e.g.
    /// "N scripts have their own inputs — query them by name"). Rendered as a
    /// plain `notes` block, without the warning glyph. Omitted from JSON when
    /// empty.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
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
            notes: Vec::new(),
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

    pub fn with_notes(mut self, notes: Vec<String>) -> Self {
        self.notes = notes;
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

    /// The human view: a summary header, then the results grouped into
    /// blank-line-separated sections with aligned columns, then a `notes` block
    /// for warnings. Sections come from each row's optional `group`; columns
    /// from its optional `cols` (falling back to `label`). The structure is what
    /// gives the output its hierarchy — see the module-level renderer helpers.
    fn render_human(&self) -> String {
        let mut blocks: Vec<String> = Vec::new();
        if !self.summary.is_empty() {
            blocks.push(self.summary.clone());
        }

        for (name, rows) in group_rows(&self.results) {
            let mut block = String::new();
            if let Some(name) = name {
                // `group (n)` — the classifier each row used to repeat, hoisted
                // to a header with its count.
                block.push_str(&format!("{name} ({})\n", rows.len()));
            }
            block.push_str(&render_section(&rows));
            blocks.push(block);
        }

        if self.results.is_empty() && self.summary.is_empty() {
            blocks.push("no results".to_string());
        }

        if !self.notes.is_empty() {
            let mut block = String::from("notes");
            for n in &self.notes {
                block.push_str("\n  ");
                block.push_str(n);
            }
            blocks.push(block);
        }

        if !self.warnings.is_empty() {
            let mut notes = String::from("notes");
            for w in &self.warnings {
                notes.push_str("\n  ⚠ ");
                notes.push_str(w);
            }
            blocks.push(notes);
        }

        let mut out = blocks.join("\n\n");
        out.push('\n');
        out
    }
}

/// Partition results into ordered sections by their `group` field, preserving
/// first-seen order (verbs sort rows into the order they want sections shown).
/// Rows with no `group` collect into one leading, header-less section.
fn group_rows(results: &[Value]) -> Vec<(Option<String>, Vec<&Value>)> {
    let mut groups: Vec<(Option<String>, Vec<&Value>)> = Vec::new();
    for r in results {
        let key = r.get("group").and_then(Value::as_str).map(str::to_string);
        match groups.iter_mut().find(|(k, _)| *k == key) {
            Some((_, rows)) => rows.push(r),
            None => groups.push((key, vec![r])),
        }
    }
    groups
}

/// Render one section's rows as aligned, two-space-indented lines: a `loc`
/// column padded to the section's widest, then the body columns (`cols`, or a
/// one-element fallback to `label`) each padded to their column's widest. The
/// last column isn't padded, so there's no trailing whitespace. A row missing a
/// column (e.g. a runtime-only effect with no `loc`) just leaves it blank.
fn render_section(rows: &[&Value]) -> String {
    let cells: Vec<(String, Vec<String>)> = rows
        .iter()
        .map(|r| {
            let loc = r.get("loc").and_then(Value::as_str).unwrap_or("").to_string();
            let cols = match r.get("cols").and_then(Value::as_array) {
                Some(a) => a
                    .iter()
                    .map(|c| c.as_str().unwrap_or("").to_string())
                    .collect(),
                None => match r.get("label").and_then(Value::as_str) {
                    Some(l) => vec![l.to_string()],
                    None => Vec::new(),
                },
            };
            (loc, cols)
        })
        .collect();

    let width = |s: &str| s.chars().count();
    let loc_w = cells.iter().map(|(l, _)| width(l)).max().unwrap_or(0);
    let n_cols = cells.iter().map(|(_, c)| c.len()).max().unwrap_or(0);
    let mut col_w = vec![0usize; n_cols];
    for (_, cols) in &cells {
        for (i, s) in cols.iter().enumerate() {
            col_w[i] = col_w[i].max(width(s));
        }
    }

    let pad = |s: &str, w: usize| {
        let mut out = s.to_string();
        for _ in width(s)..w {
            out.push(' ');
        }
        out
    };

    let mut lines = Vec::new();
    for (loc, cols) in &cells {
        let mut parts: Vec<String> = Vec::new();
        if loc_w > 0 {
            parts.push(pad(loc, loc_w));
        }
        for (i, s) in cols.iter().enumerate() {
            // Pad every column but the last present one (no trailing fill).
            if i + 1 < cols.len() {
                parts.push(pad(s, col_w[i]));
            } else {
                parts.push(s.clone());
            }
        }
        lines.push(format!("  {}", parts.join("  ")).trim_end().to_string());
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn human(query: Value, results: Vec<Value>, warnings: Vec<String>) -> String {
        Envelope::new(query, results)
            .with_summary("summary")
            .with_warnings(warnings)
            .render(Channel::Human)
    }

    #[test]
    fn falls_back_to_loc_and_label_when_ungrouped() {
        // No `group`/`cols`: one header-less section, loc + label, still aligned.
        let out = human(
            json!({"kind": "x"}),
            vec![
                json!({"loc": "a.py:1:1", "label": "fs open"}),
                json!({"loc": "bbb.py:10:2", "label": "net get"}),
            ],
            vec![],
        );
        assert_eq!(
            out,
            "summary\n\n  a.py:1:1     fs open\n  bbb.py:10:2  net get\n"
        );
    }

    #[test]
    fn groups_into_sections_with_counts_and_aligned_columns() {
        let out = human(
            json!({"kind": "describe"}),
            vec![
                json!({"loc": "a.py:1:1", "group": "callers", "cols": ["pkg.a.f"]}),
                json!({"loc": "longer/path.py:9:9", "group": "callers", "cols": ["pkg.b.g"]}),
                json!({"loc": "t.py:3:1", "group": "tests", "cols": ["pkg.t.test_x", "depth 2"]}),
            ],
            vec![],
        );
        let expected = "summary\n\n\
            callers (2)\n\
            \x20 a.py:1:1            pkg.a.f\n\
            \x20 longer/path.py:9:9  pkg.b.g\n\n\
            tests (1)\n\
            \x20 t.py:3:1  pkg.t.test_x  depth 2\n";
        assert_eq!(out, expected);
    }

    #[test]
    fn warnings_render_as_a_notes_block() {
        let out = human(
            json!({"kind": "x"}),
            vec![json!({"loc": "a.py:1:1", "label": "f"})],
            vec!["over-approximate".to_string()],
        );
        assert!(out.ends_with("notes\n  ⚠ over-approximate\n"), "{out:?}");
    }

    #[test]
    fn a_row_without_loc_still_aligns() {
        // A runtime-only observation (no loc) renders its columns, not JSON.
        let out = human(
            json!({"kind": "shapes"}),
            vec![json!({"cols": ["pkg.ops.f", "int | str"]})],
            vec![],
        );
        assert_eq!(out, "summary\n\n  pkg.ops.f  int | str\n");
    }
}
