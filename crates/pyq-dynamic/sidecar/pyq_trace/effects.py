"""Map CPython audit events onto pyq's effect taxonomy.

pyq's static `effects` verb classifies side effects into:
    fs, network, subprocess, env, db, random, clock, global
(see crates/pyq-resolve, DESIGN.md #5). The audit hook
(`sys.addaudithook`, CPython 3.8+) fires named events from inside the
interpreter and stdlib for security-relevant operations; we route the ones that
correspond to those categories.

HONESTY — what the audit hook can and cannot see:
  Covered: fs (file opens / os.* mutations), network (socket.*), subprocess
  (Popen / os.system / exec / fork), db (sqlite3.connect; other DBs ride
  network), env-WRITES (os.putenv / os.unsetenv), import.
  NOT covered: env-READS (os.getenv / environ[...] are plain dict reads, never
  audited), random, clock, and module-global mutation. Those have no audit
  event and are deferred to the `sys.monitoring` call-target seam (Phase 4+).
  The ledger surfaces this gap as a warning so a consumer never reads "no env
  effect" as "this code reads no env" — the static `effects`/`inputs` verbs
  remain the oracle for reads.
"""
from __future__ import annotations

from typing import Optional

# Exact audit event -> effect category. Prefix rules handle the families below.
_EXACT: dict[str, str] = {
    "open": "fs",
    "os.open": "fs",
    "os.mkdir": "fs",
    "os.rmdir": "fs",
    "os.remove": "fs",
    "os.unlink": "fs",
    "os.rename": "fs",
    "os.replace": "fs",
    "os.chmod": "fs",
    "os.chown": "fs",
    "os.truncate": "fs",
    "os.scandir": "fs",
    "shutil.copyfile": "fs",
    "shutil.move": "fs",
    "shutil.rmtree": "fs",
    "tempfile.mkstemp": "fs",
    "os.system": "subprocess",
    "os.fork": "subprocess",
    "os.forkpty": "subprocess",
    "subprocess.Popen": "subprocess",
    "sqlite3.connect": "db",
    "sqlite3.connect/handle": "db",
    "os.putenv": "env",
    "os.unsetenv": "env",
    "import": "import",
    "urllib.Request": "network",
}

# event prefix -> category (checked after exact misses, longest-prefix wins).
_PREFIX: tuple[tuple[str, str], ...] = (
    ("socket.", "network"),
    ("os.exec", "subprocess"),
    ("os.spawn", "subprocess"),
    ("os.posix_spawn", "subprocess"),
    ("ssl.", "network"),
)

# Effect categories pyq knows about that the audit hook structurally cannot
# observe — reported in the ledger envelope so a 0 is never misread.
UNAUDITED_CATEGORIES: tuple[str, ...] = ("env-read", "random", "clock", "global")


def categorize(event: str) -> Optional[str]:
    """Return the pyq effect category for an audit event, or None to ignore it."""
    hit = _EXACT.get(event)
    if hit is not None:
        return hit
    best: Optional[str] = None
    best_len = -1
    for prefix, cat in _PREFIX:
        if event.startswith(prefix) and len(prefix) > best_len:
            best, best_len = cat, len(prefix)
    return best
