//! Walk a directory tree, parsing every `.py` file into a [`FileIndex`].
//!
//! Uses `ignore` so `.gitignore` is respected — an agent querying a repo
//! shouldn't get hits from `.venv` or build artifacts.

use anyhow::Result;
use ignore::WalkBuilder;
use pyq_index::FileIndex;
use std::path::Path;

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

fn is_python(path: &Path) -> bool {
    path.is_file()
        && matches!(
            path.extension().and_then(|e| e.to_str()),
            Some("py") | Some("pyi")
        )
}
