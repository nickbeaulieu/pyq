//! Walk a directory tree, parsing every `.py` file into a [`FileIndex`].
//!
//! Uses `ignore` so `.gitignore` is respected — an agent querying a repo
//! shouldn't get hits from `.venv` or build artifacts.

use anyhow::Result;
use ignore::WalkBuilder;
use pyq_index::FileIndex;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

pub fn index_tree(root: &str) -> Result<Vec<FileIndex>> {
    let mut files = Vec::new();
    for entry in WalkBuilder::new(root).build() {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        let path = entry.path();
        if !is_python(path) {
            continue;
        }
        let source = match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let rel = path
            .strip_prefix(root)
            .unwrap_or(path)
            .to_string_lossy()
            .into_owned();
        files.push(crate::extract_file(&rel, &source));
    }
    Ok(files)
}

/// The set of Python files the walk includes under `root`, as canonical
/// absolute paths. This is the *file discipline* — `.gitignore`/hidden-dir
/// filtering and root scoping — that the ty engine must inherit so its results
/// match the syntactic path (one project, no nested-worktree duplicates).
pub fn walked_py_files(root: &str) -> HashSet<PathBuf> {
    let mut set = HashSet::new();
    for entry in WalkBuilder::new(root).build().flatten() {
        let path = entry.path();
        if is_python(path) {
            if let Ok(canon) = path.canonicalize() {
                set.insert(canon);
            }
        }
    }
    set
}

fn is_python(path: &Path) -> bool {
    path.is_file()
        && matches!(
            path.extension().and_then(|e| e.to_str()),
            Some("py") | Some("pyi")
        )
}
