"""Audit-event -> effect-category mapping."""
import os
import sys

sys.path.insert(0, os.path.join(os.path.dirname(__file__), ".."))

from pyq_trace.effects import categorize  # noqa: E402


def test_fs_events():
    assert categorize("open") == "fs"
    assert categorize("os.remove") == "fs"


def test_network_prefix():
    assert categorize("socket.connect") == "network"
    assert categorize("socket.__new__") == "network"
    assert categorize("ssl.wrap_socket") == "network"


def test_subprocess_family():
    assert categorize("subprocess.Popen") == "subprocess"
    assert categorize("os.system") == "subprocess"
    assert categorize("os.exec") == "subprocess"
    assert categorize("os.spawnv") == "subprocess"


def test_db_sqlite():
    assert categorize("sqlite3.connect") == "db"


def test_env_writes_only():
    assert categorize("os.putenv") == "env"
    # env READS are not audited at all -> no event reaches categorize.


def test_unrelated_event_ignored():
    assert categorize("object.__getattr__") is None
    assert categorize("compile") is None
