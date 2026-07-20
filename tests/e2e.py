#!/usr/bin/env python3
"""End-to-end test: do multiple clients really share one server process?

Drives the real `drey serve` shim over stdio, exactly as an editor or agent
would, against the mock server in mock_server.py.
"""
import json
import os
import subprocess
import sys
import tempfile
import time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
MOCK = os.path.join(ROOT, "tests", "mock_server.py")

def find_drey():
    """Locate the built binary; the target dir may be redirected."""
    out = subprocess.run(
        ["cargo", "metadata", "--format-version", "1", "--no-deps"],
        cwd=ROOT, capture_output=True, text=True,
    )
    target = json.loads(out.stdout)["target_directory"] if out.returncode == 0 \
        else os.path.join(ROOT, "target")
    path = os.path.join(target, "debug", "drey")
    if not os.path.exists(path):
        sys.exit(f"drey binary not found at {path}; run `cargo build` first")
    return path

DREY = find_drey()

failures = []

def check(label, ok, detail=""):
    print(("  PASS  " if ok else "  FAIL  ") + label + (("  " + detail) if detail and not ok else ""))
    if not ok:
        failures.append(label)

class Client:
    """An LSP client talking to a `drey serve` shim over stdio."""

    def __init__(self, env, root, name):
        self.name = name
        self.proc = subprocess.Popen(
            [DREY, "serve", "mock"],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
            cwd=root, env=env,
        )
        self.next_id = 1

    def send(self, payload):
        body = json.dumps(payload).encode()
        self.proc.stdin.write(b"Content-Length: %d\r\n\r\n" % len(body) + body)
        self.proc.stdin.flush()

    def read(self):
        length = None
        while True:
            line = self.proc.stdout.readline()
            if not line:
                return None
            line = line.decode().strip()
            if line == "":
                break
            if line.lower().startswith("content-length:"):
                length = int(line.split(":", 1)[1].strip())
        if length is None:
            return None
        return json.loads(self.proc.stdout.read(length).decode())

    def request(self, method, params=None):
        rid = self.next_id
        self.next_id += 1
        self.send({"jsonrpc": "2.0", "id": rid, "method": method, "params": params or {}})
        # Skip notifications and server-initiated requests until our reply lands.
        while True:
            msg = self.read()
            if msg is None:
                raise RuntimeError(f"{self.name}: connection closed awaiting {method}")
            if msg.get("id") == rid and "method" not in msg:
                return msg
            if "id" in msg and "method" in msg:
                self.pending_server_request = msg
                self.send({"jsonrpc": "2.0", "id": msg["id"], "result": [{"ok": True}]})

    def notify(self, method, params):
        self.send({"jsonrpc": "2.0", "method": method, "params": params})

    def initialize(self, root):
        return self.request("initialize", {
            "processId": os.getpid(),
            "rootUri": "file://" + root,
            "capabilities": {},
        })

    def close(self):
        try:
            self.proc.stdin.close()
        except Exception:
            pass
        self.proc.wait(timeout=5)


def main():
    tmp = tempfile.mkdtemp(prefix="drey-e2e-")
    runtime = os.path.join(tmp, "run")
    tally = os.path.join(tmp, "tally")
    ws = os.path.join(tmp, "ws")
    crate = os.path.join(ws, "crates", "inner")
    os.makedirs(crate)
    open(os.path.join(ws, "Cargo.toml"), "w").write("[workspace]\n")
    open(os.path.join(crate, "Cargo.toml"), "w").write("[package]\n")

    cfg = os.path.join(tmp, "config.toml")
    with open(cfg, "w") as fh:
        fh.write(f'''
[server.mock]
command = "python3"
args = ["{MOCK}"]
root_markers = ["Cargo.toml"]

[server.mock.env]
MOCK_TALLY = "{tally}"

[daemon]
idle_timeout_secs = 3600
''')

    env = dict(os.environ, DREY_CONFIG=cfg, DREY_RUNTIME_DIR=runtime, DREY_LOG="drey=debug")

    def starts():
        if not os.path.exists(tally):
            return 0
        return len(open(tally).read().strip().splitlines())

    print("\ndrey end-to-end\n")
    uri = "file://" + os.path.join(ws, "src", "main.rs")

    try:
        a = Client(env, ws, "A")
        a.initialize(ws)
        pid_a = a.request("test/ping")["result"]["pid"]

        b = Client(env, ws, "B")
        b.initialize(ws)
        pid_b = b.request("test/ping")["result"]["pid"]

        check("two clients share one server process", pid_a == pid_b, f"{pid_a} vs {pid_b}")
        check("only one server was ever spawned", starts() == 1, f"spawned {starts()}")

        # A client opening a sub-crate of an indexed workspace must attach to
        # the existing backend rather than spawn a second index.
        c = Client(env, crate, "C")
        c.initialize(crate)
        pid_c = c.request("test/ping")["result"]["pid"]
        check("a sub-crate attaches to the workspace backend", pid_c == pid_a, f"{pid_c} vs {pid_a}")
        check("still only one server process", starts() == 1, f"spawned {starts()}")

        # Server-initiated requests must reach a client and be answered.
        check("server-to-client request was delivered",
              getattr(a, "pending_server_request", None) is not None)

        # Shared reads: one client opens, the other sees the same document.
        a.notify("textDocument/didOpen", {"textDocument": {
            "uri": uri, "languageId": "rust", "version": 1, "text": "fn main() {}"}})
        time.sleep(0.3)
        check("document reached the shared server", uri in b.request("test/ping")["result"]["open"])

        # Same content from a second client must not re-open on the server, and
        # must not fork.
        b.notify("textDocument/didOpen", {"textDocument": {
            "uri": uri, "languageId": "rust", "version": 1, "text": "fn main() {}"}})
        time.sleep(0.3)
        check("identical content does not fork", b.request("test/ping")["result"]["pid"] == pid_a)

        # The owner edits: writes through to the shared copy.
        a.notify("textDocument/didChange", {
            "textDocument": {"uri": uri, "version": 2},
            "contentChanges": [{"range": {"start": {"line": 0, "character": 11},
                                          "end": {"line": 0, "character": 11}},
                                "text": " /* a */ "}]})
        time.sleep(0.3)
        check("owner edit is visible on the shared server",
              "/* a */" in a.request("test/ping")["result"]["texts"].get(uri, ""))

        # B now edits from a state that disagrees with A. Rather than fork a
        # second server, drey should swap the shared one between them.
        b.notify("textDocument/didChange", {
            "textDocument": {"uri": uri, "version": 2},
            "contentChanges": [{"range": {"start": {"line": 0, "character": 0},
                                          "end": {"line": 0, "character": 0}},
                                "text": "// b\n"}]})
        time.sleep(0.4)
        ping_b = b.request("test/ping")["result"]
        check("a divergent edit does NOT fork", ping_b["pid"] == pid_a,
              f"{ping_b['pid']} vs {pid_a}")
        check("still exactly one server process", starts() == 1, f"spawned {starts()}")

        # The heart of it: each client sees its own text, from one process.
        check("the divergent client sees its own edit", "// b" in ping_b["texts"].get(uri, ""))

        ping_a = a.request("test/ping")["result"]
        check("the other client still sees ITS text, not B's",
              "// b" not in ping_a["texts"].get(uri, "")
              and "/* a */" in ping_a["texts"].get(uri, ""),
              repr(ping_a["texts"].get(uri, ""))[:80])

        # Swapping repeatedly must stay correct rather than drift.
        drift = None
        for i in range(4):
            tb = b.request("test/ping")["result"]["texts"].get(uri, "")
            ta = a.request("test/ping")["result"]["texts"].get(uri, "")
            if "// b" not in tb or "// b" in ta:
                drift = f"round {i}: A={ta!r} B={tb!r}"
                break
        check("repeated swapping stays consistent", drift is None, drift or "")
        check("swapping never spawned a second server", starts() == 1, f"spawned {starts()}")

        status = json.loads(subprocess.run(
            [DREY, "status", "--json"], env=env, capture_output=True, text=True).stdout)
        check("one backend serves both divergent clients", len(status) == 1,
              f"got {len(status)}")
        check("status reports the document as dirty", status[0]["dirty_docs"] == 1,
              f"got {status[0]['dirty_docs']}")
        check("status counts the swaps", status[0]["swaps"] > 0, f"got {status[0]['swaps']}")

        # Saving collapses the overlay: what B wrote to disk becomes the text
        # everyone agrees on, so swapping should stop.
        b.notify("textDocument/didSave", {"textDocument": {"uri": uri}})
        time.sleep(0.3)
        # A re-reads; its own stale buffer is now the overlay, so it swaps once
        # more, then B's save means B needs no overlay at all.
        a.notify("textDocument/didClose", {"textDocument": {"uri": uri}})
        time.sleep(0.3)
        after = json.loads(subprocess.run(
            [DREY, "status", "--json"], env=env, capture_output=True, text=True).stdout)
        check("after save and close, nothing is dirty", after[0]["dirty_docs"] == 0,
              f"got {after[0]['dirty_docs']}")
        check("the saved text is what the server holds",
              "// b" in b.request("test/ping")["result"]["texts"].get(uri, ""))
        check("saving never spawned another server", starts() == 1, f"spawned {starts()}")

        for client in (a, b, c):
            client.close()
    finally:
        subprocess.run([DREY, "stop"], env=env, capture_output=True)

    print()
    if failures:
        print(f"{len(failures)} check(s) failed: " + ", ".join(failures))
        return 1
    print("all checks passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
