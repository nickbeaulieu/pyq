//! Cross-invocation analysis cache under `~/.pyq/`.
//!
//! pyq otherwise recomputes everything per invocation — re-walk the tree,
//! re-parse every file, (later) rebuild the resolved graph from a cold ty Salsa
//! DB. The cache turns that into "the first run pays, every run after is dirt
//! cheap": a content-addressed snapshot persisted under
//! `~/.pyq/cache/<root-hash>/`, reused across invocations until the source
//! actually changes. This module owns the **parse layer** (#38.2) — the per-file
//! [`FileIndex`]. The graph and runtime-ledger layers (#38.3/#38.4) will land
//! beside it, keyed by the same fingerprint discipline established here.
//!
//! Two invariants make caching safe to do automatically (no flag):
//! - **Correctness never depends on the cache.** A parse is a pure function of
//!   file content; a stale or corrupt cache can at worst cost a re-parse. Every
//!   load/store is best-effort and falls back to [`walk::index_tree`].
//! - **Validation is cheap.** A `stat` sweep (`size` + `mtime_ns`) reusing the
//!   walk we already do; only files whose stat moved get a `blake3` content
//!   hash, so a touch-without-change doesn't force a re-parse and a clean repo
//!   hashes nothing.
//!
//! Set `PYQ_NO_CACHE` to bypass the cache entirely (tests, debugging, a
//! read-only `$HOME`).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use anyhow::Result;
use pyq_index::FileIndex;
use serde::{Deserialize, Serialize};

use crate::walk;

/// Bumped whenever the on-disk shape changes (a new `FileIndex` field, a format
/// switch). A mismatch discards the cache rather than misreading it.
const SCHEMA_VERSION: u32 = 1;

/// One cached file: the stat fingerprint that validates it plus the parsed
/// facts. `hash` lets a touch-without-change (mtime moved, bytes identical)
/// reuse the parse instead of redoing it.
#[derive(Serialize, Deserialize, Clone)]
struct CachedFile {
    size: u64,
    mtime_ns: u128,
    hash: [u8; 32],
    index: FileIndex,
}

/// The persisted parse layer: every walked file's [`FileIndex`], keyed by its
/// root-relative path.
#[derive(Serialize, Deserialize)]
struct ParseCache {
    schema: u32,
    entries: BTreeMap<String, CachedFile>,
}

impl Default for ParseCache {
    fn default() -> Self {
        ParseCache {
            schema: SCHEMA_VERSION,
            entries: BTreeMap::new(),
        }
    }
}

/// Parse every Python file under `root`, reusing cached `FileIndex`es for files
/// that haven't changed and re-parsing only the ones that have. Drop-in for
/// [`walk::index_tree`]; identical results, cheaper on repeat.
pub fn index_tree(root: &str) -> Result<Vec<FileIndex>> {
    if std::env::var_os("PYQ_NO_CACHE").is_some() {
        return walk::index_tree(root);
    }
    // The cache is an optimization, never a correctness dependency: any failure
    // (no `$HOME`, unreadable cache dir, a deserialize miss) degrades to a full
    // parse.
    match cached_index(root) {
        Ok(files) => Ok(files),
        Err(_) => walk::index_tree(root),
    }
}

fn cached_index(root: &str) -> Result<Vec<FileIndex>> {
    let dir = cache_dir(root);
    let mut prev = dir
        .as_ref()
        .and_then(|d| load(&d.join("parse.bin")))
        .filter(|c| c.schema == SCHEMA_VERSION)
        .unwrap_or_default();

    let mut next: BTreeMap<String, CachedFile> = BTreeMap::new();
    let mut out: Vec<FileIndex> = Vec::new();
    let mut dirty = false;

    for (rel, abs) in walk::py_files(root) {
        let meta = std::fs::metadata(&abs).ok();
        let size = meta.as_ref().map(|m| m.len()).unwrap_or(0);
        let mtime_ns = meta
            .as_ref()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_nanos())
            .unwrap_or(0);

        // Fast path: stat unchanged → trust the cached parse, no read, no hash.
        if let Some(hit) = prev.entries.get(&rel) {
            if hit.size == size && hit.mtime_ns == mtime_ns {
                out.push(hit.index.clone());
                next.insert(rel, hit.clone());
                continue;
            }
        }

        // Stat moved (or first sight): we need the bytes to decide.
        let source = match std::fs::read_to_string(&abs) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let hash = *blake3::hash(source.as_bytes()).as_bytes();

        // Content unchanged (touched, reverted, or just a coarse mtime): reuse
        // the parse, but refresh the stat so the next run hits the fast path.
        if let Some(hit) = prev.entries.get(&rel) {
            if hit.hash == hash {
                out.push(hit.index.clone());
                next.insert(
                    rel,
                    CachedFile {
                        size,
                        mtime_ns,
                        hash,
                        index: hit.index.clone(),
                    },
                );
                dirty = true;
                continue;
            }
        }

        // Genuinely changed (or new): re-parse.
        let index = crate::extract_file(&rel, &source);
        out.push(index.clone());
        next.insert(
            rel,
            CachedFile {
                size,
                mtime_ns,
                hash,
                index,
            },
        );
        dirty = true;
    }

    // A deletion leaves `next` smaller than `prev` with no other trigger.
    if next.len() != prev.entries.len() {
        dirty = true;
    }
    prev.entries = next;
    prev.schema = SCHEMA_VERSION;

    if dirty {
        if let Some(dir) = dir {
            let _ = store(&dir, &prev);
        }
    }
    Ok(out)
}

/// `~/.pyq/cache/<root-hash>/` — global, namespaced per repo by a hash of the
/// canonicalized root so distinct checkouts never collide. `~/.pyq/` is the
/// reserved home for anything pyq persists later (config, logs). `PYQ_CACHE_DIR`
/// overrides the `~/.pyq/cache` base (hermetic tests, sandboxes, a read-only
/// `$HOME`); the per-root namespacing still applies underneath it.
fn cache_dir(root: &str) -> Option<PathBuf> {
    let base = match std::env::var_os("PYQ_CACHE_DIR") {
        Some(d) => PathBuf::from(d),
        None => dirs::home_dir()?.join(".pyq").join("cache"),
    };
    let canon = std::fs::canonicalize(root).unwrap_or_else(|_| PathBuf::from(root));
    let key = blake3::hash(canon.to_string_lossy().as_bytes()).to_hex();
    Some(base.join(key.as_str()))
}

fn load(path: &Path) -> Option<ParseCache> {
    let bytes = std::fs::read(path).ok()?;
    bincode::deserialize(&bytes).ok()
}

/// Write the cache atomically: serialize to a pid-unique temp in the cache dir,
/// then `rename` over `parse.bin`. The rename is atomic on the same filesystem,
/// so a concurrent reader never sees a torn file and concurrent writers at worst
/// duplicate work (last writer wins, both products valid).
fn store(dir: &Path, cache: &ParseCache) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    let bytes = bincode::serialize(cache)?;
    let tmp = dir.join(format!("parse.bin.{}.tmp", std::process::id()));
    std::fs::write(&tmp, &bytes)?;
    std::fs::rename(&tmp, dir.join("parse.bin"))?;
    Ok(())
}
