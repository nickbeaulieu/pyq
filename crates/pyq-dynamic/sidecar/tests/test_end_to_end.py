"""End-to-end: run a real target under the sidecar in a fresh interpreter and
assert the emitted envelope. Subprocess-based so the persistent audit hook never
leaks into the test process."""
import json
import os
import subprocess
import sys
import textwrap

SIDECAR = os.path.abspath(os.path.join(os.path.dirname(__file__), ".."))

TARGET = textwrap.dedent(
    """
    import os, socket

    def touches_fs():
        with open(os.devnull) as f:
            return f.read()

    def touches_net():
        s = socket.socket()
        try:
            s.connect(("127.0.0.1", 9))
        except OSError:
            pass
        finally:
            s.close()

    def main():
        touches_fs()
        touches_net()

    if __name__ == "__main__":
        main()
    """
)


def run_sidecar(root, script, tmp_path):
    env = {**os.environ, "PYTHONPATH": SIDECAR}
    out = os.path.join(str(tmp_path), "_ledger.json")
    proc = subprocess.run(
        [sys.executable, "-m", "pyq_trace", "--root", root,
         "--script", script, "--out", out],
        capture_output=True, text=True, env=env,
    )
    assert proc.returncode == 0, proc.stderr
    with open(out) as fh:
        return json.load(fh)


def test_observes_fs_and_network(tmp_path):
    script = tmp_path / "app.py"
    script.write_text(TARGET)
    env = run_sidecar(str(tmp_path), str(script), tmp_path)

    assert env["tool"] == "pyq"
    assert env["query"]["kind"] == "effects-observed"
    by_owner_effect = {(r["owner"], r["effect"]) for r in env["results"]}

    # fs attributed to the function that opened the file, not to stdlib `io`.
    assert ("app.touches_fs", "fs") in by_owner_effect
    # network attributed to the socket caller.
    assert ("app.touches_net", "network") in by_owner_effect


def test_warns_about_unaudited_categories(tmp_path):
    script = tmp_path / "app.py"
    script.write_text(TARGET)
    env = run_sidecar(str(tmp_path), str(script), tmp_path)
    blob = " ".join(env["warnings"]).lower()
    assert "env-read" in blob and "random" in blob and "clock" in blob


def test_loader_source_reads_are_not_logged_as_fs(tmp_path):
    # Importing a helper module must not show up as the importer `open`ing a .py.
    pkg = tmp_path / "helper.py"
    pkg.write_text("VALUE = 1\n")
    script = tmp_path / "app.py"
    script.write_text("import helper\nif __name__ == '__main__':\n    print(helper.VALUE)\n")
    env = run_sidecar(str(tmp_path), str(script), tmp_path)
    fs_rows = [r for r in env["results"] if r["effect"] == "fs"]
    assert fs_rows == [], fs_rows


def test_clean_target_yields_empty_ledger(tmp_path):
    script = tmp_path / "pure.py"
    script.write_text("def f(x):\n    return x + 1\n\nif __name__ == '__main__':\n    f(1)\n")
    env = run_sidecar(str(tmp_path), str(script), tmp_path)
    assert env["count"] == 0
    assert env["results"] == []
