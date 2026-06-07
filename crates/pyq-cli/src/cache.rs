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
/// the `GraphRecording` shape changes.
const GRAPH_SCHEMA: u32 = 1;

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

/// The persisted graph layer: the recorded ty edges plus the fingerprint they
/// were recorded against. A fingerprint mismatch means the source moved, so the
/// recording is stale and rebuilt.
#[derive(Serialize, Deserialize)]
struct GraphCache {
    schema: u32,
    fingerprint: String,
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
    let dir = cache_dir(root);

    if !fingerprint.is_empty() {
        if let Some(gc) = dir.as_ref().and_then(|d| read_bin::<GraphCache>(&d.join("graph.bin"))) {
            if gc.schema == GRAPH_SCHEMA && gc.fingerprint == fingerprint {
                return Ok(CallGraph::replay(files.to_vec(), gc.recording));
            }
        }
    }

    let graph = CallGraph::new(root, files.to_vec(), scope)?;
    if !fingerprint.is_empty() {
        if let Some(dir) = dir {
            let cache = GraphCache {
                schema: GRAPH_SCHEMA,
                fingerprint: fingerprint.to_string(),
                recording: graph.record(),
            };
            let _ = write_bin(&dir, "graph.bin", &cache);
        }
    }
    Ok(graph)
}

/// Independent version for the runtime-ledger layer (observed effects from a
/// suite run). Bumped when the stored shape changes.
const LEDGER_SCHEMA: u32 = 1;

/// The persisted runtime ledger: the `(owner FQN, effect category)` pairs the
/// suite actually performed, keyed by the tree fingerprint it was observed
/// against.
#[derive(Serialize, Deserialize)]
struct LedgerCache {
    schema: u32,
    fingerprint: String,
    effects: Vec<(String, String)>,
    pytest_exit: Option<i64>,
    warnings: Vec<String>,
}

/// What a suite run observed — or why it couldn't. `available` is false when the
/// suite was skipped (`PYQ_NO_SUITE`) or couldn't run (no interpreter/pytest);
/// callers degrade to static `predicted` rather than erroring.
pub struct ObservedEffects {
    pub pairs: HashSet<(String, String)>,
    pub available: bool,
    pub pytest_exit: Option<i64>,
    pub warnings: Vec<String>,
}

/// The observed effect ledger for `root`: served from `ledger.bin` when its
/// fingerprint matches, otherwise the suite is **run on demand** and the result
/// cached. Set `PYQ_NO_SUITE` to skip the run entirely (CI, sandboxes, or when
/// the static prediction is enough). A failed run degrades to `available:false`
/// and is not cached, so the next call retries.
pub fn ledger_effects(root: &str, fingerprint: &str, python: &str) -> ObservedEffects {
    let unavailable = |warning: String| ObservedEffects {
        pairs: HashSet::new(),
        available: false,
        pytest_exit: None,
        warnings: vec![warning],
    };

    if std::env::var_os("PYQ_NO_SUITE").is_some() {
        return unavailable(
            "suite run skipped (PYQ_NO_SUITE set) — effects are static predictions".to_string(),
        );
    }

    let dir = cache_dir(root);
    if !fingerprint.is_empty() {
        if let Some(lc) = dir.as_ref().and_then(|d| read_bin::<LedgerCache>(&d.join("ledger.bin"))) {
            if lc.schema == LEDGER_SCHEMA && lc.fingerprint == fingerprint {
                return ObservedEffects {
                    pairs: lc.effects.into_iter().collect(),
                    available: true,
                    pytest_exit: lc.pytest_exit,
                    warnings: lc.warnings,
                };
            }
        }
    }

    // Cache miss → run the suite under the dynamic tier.
    let mut opts = pyq_dynamic::TraceOptions::new(root.to_string());
    opts.python = python.to_string();
    let env = match pyq_dynamic::observed_effects(&opts) {
        Ok(env) => env,
        Err(e) => {
            return unavailable(format!(
                "could not run the test suite ({e}) — effects are static predictions"
            ))
        }
    };

    let mut pairs = HashSet::new();
    for r in &env.results {
        if let (Some(owner), Some(cat)) = (
            r.get("owner").and_then(|v| v.as_str()),
            r.get("effect").and_then(|v| v.as_str()),
        ) {
            // `import` is a load event, not one of the static effect categories.
            if cat != "import" {
                pairs.insert((owner.to_string(), cat.to_string()));
            }
        }
    }
    let pytest_exit = env.query.get("pytest_exit").and_then(|v| v.as_i64());
    let warnings = env.warnings.clone();

    if !fingerprint.is_empty() {
        if let Some(dir) = dir {
            let lc = LedgerCache {
                schema: LEDGER_SCHEMA,
                fingerprint: fingerprint.to_string(),
                effects: pairs.iter().cloned().collect(),
                pytest_exit,
                warnings: warnings.clone(),
            };
            let _ = write_bin(&dir, "ledger.bin", &lc);
        }
    }

    ObservedEffects {
        pairs,
        available: true,
        pytest_exit,
        warnings,
    }
}

/// Independent version for the observed-shapes layer (runtime return types).
const SHAPES_SCHEMA: u32 = 1;

#[derive(Serialize, Deserialize)]
struct ShapesCache {
    schema: u32,
    fingerprint: String,
    returns: Vec<(String, Vec<String>)>,
}

/// The runtime return types each callable produced, keyed by FQN. Empty when the
/// suite was skipped/failed or the interpreter is pre-3.12 (`sys.monitoring`
/// absent) — callers simply omit the observed column in that case.
pub struct ObservedShapes {
    pub returns: BTreeMap<String, Vec<String>>,
}

/// The observed-shapes ledger for `root`: served from `shapes.bin` on a
/// fingerprint match, else the suite is run on demand and cached. `PYQ_NO_SUITE`
/// skips it; a pre-3.12 / failed run yields an empty map and isn't cached (so a
/// later capable interpreter retries).
pub fn ledger_shapes(root: &str, fingerprint: &str, python: &str) -> ObservedShapes {
    let empty = || ObservedShapes {
        returns: BTreeMap::new(),
    };
    if std::env::var_os("PYQ_NO_SUITE").is_some() {
        return empty();
    }
    let dir = cache_dir(root);
    if !fingerprint.is_empty() {
        if let Some(sc) = dir.as_ref().and_then(|d| read_bin::<ShapesCache>(&d.join("shapes.bin"))) {
            if sc.schema == SHAPES_SCHEMA && sc.fingerprint == fingerprint {
                return ObservedShapes {
                    returns: sc.returns.into_iter().collect(),
                };
            }
        }
    }
    let mut opts = pyq_dynamic::TraceOptions::new(root.to_string());
    opts.python = python.to_string();
    let shapes = match pyq_dynamic::observed_shapes(&opts) {
        Ok(s) if s.monitoring_available => s,
        _ => return empty(),
    };
    if !fingerprint.is_empty() {
        if let Some(dir) = dir {
            let sc = ShapesCache {
                schema: SHAPES_SCHEMA,
                fingerprint: fingerprint.to_string(),
                returns: shapes.returns.clone().into_iter().collect(),
            };
            let _ = write_bin(&dir, "shapes.bin", &sc);
        }
    }
    ObservedShapes {
        returns: shapes.returns,
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
