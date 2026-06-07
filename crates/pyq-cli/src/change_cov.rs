//! Parse `git diff` into the set of changed lines per file, keyed by path
//! relative to the scan root — the input half of change-coverage (#9.4). The
//! coverage half (per-test executed lines) comes from `pyq-dynamic`; joining
//! the two tells which changed lines a test actually exercises.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result};

/// Changed (added/modified) lines per project-relative `.py` file, computed
/// from `git diff --unified=0 <base>` run in `root`. "Changed" = lines present
/// in the *new* version (the ones a test would need to execute), so deletions
/// contribute nothing. Paths are relativized to `root` so they join directly
/// with coverage keys; files outside `root` are dropped.
pub fn changed_lines(root: &str, base: &str) -> Result<BTreeMap<String, BTreeSet<u32>>> {
    let toplevel = git_toplevel(root)?;
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["diff", "--unified=0", "--no-color", base, "--", "*.py"])
        .output()
        .context("running `git diff` (is git installed and is this a repo?)")?;
    if !out.status.success() {
        anyhow::bail!(
            "`git diff {base}` failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    parse_diff(&String::from_utf8_lossy(&out.stdout), &toplevel, root)
}

fn git_toplevel(root: &str) -> Result<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .context("running `git rev-parse` (is git installed?)")?;
    if !out.status.success() {
        anyhow::bail!(
            "not a git repository (or git unavailable): {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Parse unified-diff text. `toplevel` is the git repo root (diff paths are
/// relative to it); `root` is the scan root we want keys relative to.
fn parse_diff(
    diff: &str,
    toplevel: &str,
    root: &str,
) -> Result<BTreeMap<String, BTreeSet<u32>>> {
    let mut changed: BTreeMap<String, BTreeSet<u32>> = BTreeMap::new();
    let mut current: Option<String> = None;

    for line in diff.lines() {
        if let Some(path) = line.strip_prefix("+++ ") {
            // `+++ b/pkg/models.py` (or `/dev/null` for a deletion).
            current = relativize(path, toplevel, root);
        } else if line.starts_with("@@") {
            if let (Some(file), Some((start, count))) = (&current, hunk_new_range(line)) {
                let entry = changed.entry(file.clone()).or_default();
                for ln in start..start + count.max(1) {
                    entry.insert(ln);
                }
            }
        }
    }
    Ok(changed)
}

/// `+++ b/path` -> path relative to `root`, or None for `/dev/null` / outside.
fn relativize(plus_path: &str, toplevel: &str, root: &str) -> Option<String> {
    let raw = plus_path.split('\t').next().unwrap_or(plus_path).trim();
    let repo_rel = raw.strip_prefix("b/").unwrap_or(raw);
    if repo_rel == "/dev/null" {
        return None;
    }
    let abs = Path::new(toplevel).join(repo_rel);
    let rel = pathdiff(&abs, Path::new(root))?;
    if rel.starts_with("..") {
        None
    } else {
        Some(rel.replace('\\', "/"))
    }
}

/// `@@ -a,b +c,d @@` -> (c, d). `d` defaults to 1 when omitted (`+c`). A pure
/// deletion hunk is `+c,0` -> count 0, contributing no changed lines.
fn hunk_new_range(header: &str) -> Option<(u32, u32)> {
    let plus = header.split('+').nth(1)?;
    let spec = plus.split(|c| c == ' ' || c == '@').next()?;
    let mut parts = spec.split(',');
    let start: u32 = parts.next()?.trim().parse().ok()?;
    let count: u32 = match parts.next() {
        Some(c) => c.trim().parse().ok()?,
        None => 1,
    };
    Some((start, count))
}

/// Minimal path-relative without an extra crate: relpath of `abs` from `base`,
/// assuming both are absolute and lexically normalized enough for our use.
fn pathdiff(abs: &Path, base: &Path) -> Option<String> {
    let abs = abs.canonicalize().ok()?;
    let base = base.canonicalize().ok()?;
    let rel = abs.strip_prefix(&base).ok()?;
    Some(rel.to_string_lossy().into_owned())
}
