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
use std::collections::HashMap;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use anyhow::Result;
use pyq_index::FileIndex;
use pyq_resolve::{CallGraph, GraphRecording};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::walk;

/// Bumped whenever the on-disk parse shape changes (a new `FileIndex` field, a
/// format switch). A mismatch discards the cache rather than misreading it.
const SCHEMA_VERSION: u32 = 2;

/// Independent version for the graph layer (the recorded ty edges). Bumped when
/// the `GraphRecording` shape or how it's built changes.
const GRAPH_SCHEMA: u32 = 3;

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
    Ok(indexed(root)?.0)
}

/// Like [`index_tree`], but also returns the **whole-tree fingerprint** — a hash
/// over every file's content hash, the key the graph and ledger layers validate
/// against. Empty string when caching is off (`PYQ_NO_CACHE` or a fall-back
/// parse), which downstream layers read as "don't cache."
pub fn indexed(root: &str) -> Result<(Vec<FileIndex>, String)> {
    if std::env::var_os("PYQ_NO_CACHE").is_some() {
        return Ok((walk::index_tree(root)?, String::new()));
    }
    // The cache is an optimization, never a correctness dependency: any failure
    // (no `$HOME`, unreadable cache dir, a deserialize miss) degrades to a full
    // parse with no fingerprint.
    match cached_index(root) {
        Ok(pair) => Ok(pair),
        Err(_) => Ok((walk::index_tree(root)?, String::new())),
    }
}

fn cached_index(root: &str) -> Result<(Vec<FileIndex>, String)> {
    let dir = cache_dir(root);
    let mut prev: ParseCache = dir
        .as_ref()
        .and_then(|d| read_bin(&d.join("parse.bin")))
        .filter(|c: &ParseCache| c.schema == SCHEMA_VERSION)
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

    // Whole-tree fingerprint: a hash over every file's (path, content hash) in
    // sorted order (`next` is a BTreeMap, so iteration is sorted). Identical
    // parse output → identical fingerprint, independent of mtimes.
    let mut hasher = blake3::Hasher::new();
    hasher.update(&SCHEMA_VERSION.to_le_bytes());
    for (rel, cf) in &next {
        hasher.update(rel.as_bytes());
        hasher.update(&[0]);
        hasher.update(&cf.hash);
    }
    let fingerprint = hasher.finalize().to_hex().to_string();

    prev.entries = next;
    prev.schema = SCHEMA_VERSION;

    if dirty {
        if let Some(dir) = dir {
            let _ = write_bin(&dir, "parse.bin", &prev);
        }
    }
    Ok((out, fingerprint))
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

/// The cache directory for `root`, if one can be resolved — so the `index` verb
/// can report where it wrote and `index clean` knows what to remove.
pub fn location(root: &str) -> Option<PathBuf> {
    cache_dir(root)
}

/// Remove this repo's entire cache directory. Returns the path removed, or
/// `None` when there was nothing to remove (or no cache dir could be resolved).
pub fn clean(root: &str) -> Result<Option<PathBuf>> {
    let Some(dir) = cache_dir(root) else {
        return Ok(None);
    };
    if dir.exists() {
        std::fs::remove_dir_all(&dir)?;
        Ok(Some(dir))
    } else {
        Ok(None)
    }
}

/// The persisted graph layer: the recorded ty edges, the whole-tree fingerprint
/// they were recorded against (a hit replays as-is), and a per-file content hash
/// so a *mismatch* can be repaired incrementally — re-recording only the files
/// that changed and their import neighbours, not the whole tree (#38.5).
#[derive(Serialize, Deserialize)]
struct GraphCache {
    schema: u32,
    fingerprint: String,
    file_hashes: BTreeMap<String, [u8; 32]>,
    recording: GraphRecording,
}

/// The transitive [`CallGraph`] over `root`, cache-backed: a warm run whose
/// `fingerprint` matches the persisted recording replays it **without
/// constructing ty**; a cold run (or a stale/empty fingerprint) builds the live
/// graph, records its full ty-query surface, and persists it for next time.
///
/// `files`/`scope` are the same the caller would pass to [`CallGraph::new`].
/// Recording on a cold run is extra work over answering a single query — the
/// "first run pays" cost — but every run after is ty-free.
pub fn call_graph(
    root: &str,
    files: &[FileIndex],
    scope: HashSet<PathBuf>,
    fingerprint: &str,
) -> Result<CallGraph> {
    call_graph_with_progress(root, files, scope, fingerprint, &|_, _| {})
}

/// Like [`call_graph`], reporting record progress (for `pyq index`'s bar).
///
/// Every path answers from a [`GraphRecording`], cached or not: a fingerprint
/// hit replays the persisted one; otherwise we build it live, persist it (when
/// caching is on), and replay from it. Routing the uncached path through the
/// recording too means a `PYQ_NO_CACHE` run gives the *identical* answer to a
/// warm one — the recording, not raw ty, is the single source of truth.
pub fn call_graph_with_progress(
    root: &str,
    files: &[FileIndex],
    scope: HashSet<PathBuf>,
    fingerprint: &str,
    progress: &dyn Fn(pyq_resolve::RecordPhase, usize),
) -> Result<CallGraph> {
    // Caching off (`PYQ_NO_CACHE`): build live, record, replay — never touch disk.
    if fingerprint.is_empty() {
        let graph = CallGraph::new(root, files.to_vec(), scope)?;
        let recording = graph.record_with_progress(progress);
        return Ok(CallGraph::replay(files.to_vec(), recording));
    }

    let dir = cache_dir(root);
    let prev = dir.as_ref().and_then(|d| read_bin::<GraphCache>(&d.join("graph.bin")));
    let prev = prev.filter(|gc| gc.schema == GRAPH_SCHEMA);

    // Warm: the whole-tree fingerprint matches → replay the recording as-is.
    if let Some(gc) = &prev {
        if gc.fingerprint == fingerprint {
            return Ok(CallGraph::replay(files.to_vec(), gc.recording.clone()));
        }
    }

    let cur_hashes = file_hashes(files);
    let graph = CallGraph::new(root, files.to_vec(), scope)?;
    let recording = match prev {
        // Have a prior recording but the tree moved → re-record only the files
        // that changed and their import component (#38.5), reusing the rest.
        Some(gc) => {
            let changed = changed_files(&gc.file_hashes, &cur_hashes);
            let import_adj = file_import_adjacency(files);
            graph.record_incremental(gc.recording, &changed, &import_adj, progress)
        }
        // No prior recording → full record.
        None => graph.record_with_progress(progress),
    };

    if let Some(dir) = dir {
        let cache = GraphCache {
            schema: GRAPH_SCHEMA,
            fingerprint: fingerprint.to_string(),
            file_hashes: cur_hashes,
            recording: recording.clone(),
        };
        let _ = write_bin(&dir, "graph.bin", &cache);
    }
    Ok(CallGraph::replay(files.to_vec(), recording))
}

/// A per-file content hash keyed by path — the blake3 of each file's serialized
/// `FileIndex`, so it changes exactly when the file's parse does. The basis for
/// the incremental graph diff (which files moved since the recording).
fn file_hashes(files: &[FileIndex]) -> BTreeMap<String, [u8; 32]> {
    files
        .iter()
        .map(|f| {
            let bytes = bincode::serialize(f).unwrap_or_default();
            (f.path.clone(), *blake3::hash(&bytes).as_bytes())
        })
        .collect()
}

/// Files whose hash differs between two snapshots — modified, added, or removed.
fn changed_files(
    prev: &BTreeMap<String, [u8; 32]>,
    cur: &BTreeMap<String, [u8; 32]>,
) -> HashSet<String> {
    let mut changed = HashSet::new();
    for (path, hash) in cur {
        if prev.get(path) != Some(hash) {
            changed.insert(path.clone());
        }
    }
    for path in prev.keys() {
        if !cur.contains_key(path) {
            changed.insert(path.clone());
        }
    }
    changed
}

/// The project's **undirected** file-level import adjacency: `A ↔ B` when file
/// `A` imports a module whose file is `B`. The incremental recorder walks this to
/// find the component of changed files — every cross-file ty dependency (a call
/// resolving into another module, a base class, a caller) rides an import edge.
fn file_import_adjacency(files: &[FileIndex]) -> HashMap<String, Vec<String>> {
    let g = crate::graph::Graph::build(files);
    let mut adj: HashMap<String, Vec<String>> = HashMap::new();
    for e in &g.edges {
        if let Some(target_file) = g.file_of(&e.target) {
            if target_file != e.importer_file {
                adj.entry(e.importer_file.clone()).or_default().push(target_file.to_string());
                adj.entry(target_file.to_string()).or_default().push(e.importer_file.clone());
            }
        }
    }
    adj
}

/// Version for the runtime-ledger layer. One instrumented suite run feeds
/// effects + coverage + shapes together (#38.4); bumped when the stored shape
/// changes.
const LEDGER_SCHEMA: u32 = 2;

/// The persisted runtime ledger: everything one suite run observed, keyed by the
/// tree fingerprint. One file, one run — `effects`, `describe`, and `tests
/// --base` all read from it instead of each running pytest.
#[derive(Serialize, Deserialize)]
struct LedgerCache {
    schema: u32,
    fingerprint: String,
    effects: Vec<(String, String)>,
    coverage: pyq_dynamic::Coverage,
    shapes: BTreeMap<String, Vec<String>>,
    pytest_exit: Option<i64>,
    warnings: Vec<String>,
}

/// What one instrumented suite run observed — or why it couldn't. `available` is
/// false when the suite was skipped (`PYQ_NO_SUITE`) or couldn't run (no
/// interpreter/pytest); callers degrade rather than erroring. `effects` is the
/// `(owner, category)` set, `shapes` is observed return types by FQN, and
/// `coverage` is per-test line coverage (its own `monitoring_available` gates
/// the 3.12+ collectors; the audit-hook `effects` work on any interpreter).
pub struct Ledger {
    pub available: bool,
    pub effects: HashSet<(String, String)>,
    pub shapes: BTreeMap<String, Vec<String>>,
    pub coverage: pyq_dynamic::Coverage,
    pub pytest_exit: Option<i64>,
    pub warnings: Vec<String>,
}

fn empty_coverage() -> pyq_dynamic::Coverage {
    pyq_dynamic::Coverage {
        python: String::new(),
        monitoring_available: false,
        tests: BTreeMap::new(),
        files: BTreeMap::new(),
        pytest_exit: None,
    }
}

/// The runtime ledger for `root`: served from `ledger.bin` on a fingerprint
/// match, otherwise the suite is **run once** with every collector active and
/// the result cached — so `effects`, `describe`, and `tests --base` share a
/// single pytest run rather than one apiece. `PYQ_NO_SUITE` skips it; a failed
/// run degrades to `available:false` and isn't cached (retry next call).
pub fn ledger(root: &str, fingerprint: &str, python: &str) -> Ledger {
    let unavailable = |warning: String| Ledger {
        available: false,
        effects: HashSet::new(),
        shapes: BTreeMap::new(),
        coverage: empty_coverage(),
        pytest_exit: None,
        warnings: vec![warning],
    };

    if std::env::var_os("PYQ_NO_SUITE").is_some() {
        return unavailable(
            "suite run skipped (PYQ_NO_SUITE set) — results are static predictions".to_string(),
        );
    }

    let dir = cache_dir(root);
    if !fingerprint.is_empty() {
        if let Some(lc) = dir.as_ref().and_then(|d| read_bin::<LedgerCache>(&d.join("ledger.bin"))) {
            if lc.schema == LEDGER_SCHEMA && lc.fingerprint == fingerprint {
                return Ledger {
                    available: true,
                    effects: lc.effects.into_iter().collect(),
                    shapes: lc.shapes,
                    coverage: lc.coverage,
                    pytest_exit: lc.pytest_exit,
                    warnings: lc.warnings,
                };
            }
        }
    }

    // Cache miss → one instrumented suite run for all three ledgers.
    let mut opts = pyq_dynamic::TraceOptions::new(root.to_string());
    opts.python = python.to_string();
    let observed = match pyq_dynamic::observed_all(&opts) {
        Ok(o) => o,
        Err(e) => {
            return unavailable(format!(
                "could not run the test suite ({e}) — results are static predictions"
            ))
        }
    };

    let mut effects = HashSet::new();
    for r in &observed.effects.results {
        if let (Some(owner), Some(cat)) = (
            r.get("owner").and_then(|v| v.as_str()),
            r.get("effect").and_then(|v| v.as_str()),
        ) {
            // `import` is a load event, not one of the static effect categories.
            if cat != "import" {
                effects.insert((owner.to_string(), cat.to_string()));
            }
        }
    }
    let pytest_exit = observed.effects.query.get("pytest_exit").and_then(|v| v.as_i64());
    let warnings = observed.effects.warnings.clone();
    let shapes = observed.shapes.returns;
    let coverage = observed.coverage;

    if !fingerprint.is_empty() {
        if let Some(dir) = dir {
            let lc = LedgerCache {
                schema: LEDGER_SCHEMA,
                fingerprint: fingerprint.to_string(),
                effects: effects.iter().cloned().collect(),
                coverage: coverage.clone(),
                shapes: shapes.clone(),
                pytest_exit,
                warnings: warnings.clone(),
            };
            let _ = write_bin(&dir, "ledger.bin", &lc);
        }
    }

    Ledger {
        available: true,
        effects,
        shapes,
        coverage,
        pytest_exit,
        warnings,
    }
}

fn read_bin<T: DeserializeOwned>(path: &Path) -> Option<T> {
    let bytes = std::fs::read(path).ok()?;
    bincode::deserialize(&bytes).ok()
}

/// Write a cache blob atomically: serialize to a pid-unique temp in the cache
/// dir, then `rename` over `name`. The rename is atomic on the same filesystem,
/// so a concurrent reader never sees a torn file and concurrent writers at worst
/// duplicate work (last writer wins, both products valid).
fn write_bin<T: Serialize>(dir: &Path, name: &str, value: &T) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    let bytes = bincode::serialize(value)?;
    let tmp = dir.join(format!("{name}.{}.tmp", std::process::id()));
    std::fs::write(&tmp, &bytes)?;
    std::fs::rename(&tmp, dir.join(name))?;
    Ok(())
}
