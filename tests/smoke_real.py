#!/usr/bin/env python3
"""Smoke test against a *real* language server, not the mock.

tests/e2e.py proves the protocol handling against tests/mock_server.py, which
behaves exactly as the test expects. This one proves the part a mock cannot:
that a real incremental engine, handed one client's text and then another's,
answers each from its own version of the source.

Not run in CI. It needs a real server installed and spends ~30s indexing.

    python3 tests/smoke_real.py                       # finds rust-analyzer on PATH
    python3 tests/smoke_real.py /path/to/rust-analyzer

Pass the *real* binary, not drey's wrapper, or the daemon spawns itself.
"""
import json
import os
import shutil
import subprocess
import sys
import tempfile
import time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))

SRC = '''pub struct Widget { pub size: u32 }

impl Widget {
    pub fn area(&self) -> u32 { self.size * self.size }
}

pub fn main() {
    let w = Widget { size: 4 };
    println!("{}", w.area());
}
'''
# The one-word edit that changes the type layout, so a wrong answer is
# unmistakable: u32 is size 4, u64 is size 8.
EDITED = SRC.replace("pub size: u32", "pub size: u64")


def find_drey():
    out = subprocess.run(["cargo", "metadata", "--format-version", "1", "--no-deps"],
                         cwd=ROOT, capture_output=True, text=True)
    target = (json.loads(out.stdout)["target_directory"] if out.returncode == 0
              else os.path.join(ROOT, "target"))
    path = os.environ.get("DREY_BIN") or os.path.join(target, "debug", "drey")
    if not os.path.exists(path):
        sys.exit(f"drey binary not found at {path}; run `cargo build` first")
    return path


def find_server(argv):
    if len(argv) > 1:
        return argv[1]
    # Skip drey's own wrappers; execing one would recurse into the daemon.
    for d in os.environ.get("PATH", "").split(os.pathsep):
        cand = os.path.join(d, "rust-analyzer")
        if os.path.isfile(cand) and os.access(cand, os.X_OK):
            if "drey" in open(cand, "rb").read(400).decode("utf-8", "replace"):
                continue
            return cand
    sys.exit("no real rust-analyzer found; pass its path as an argument")


def frame(obj):
    body = json.dumps(obj).encode()
    return f"Content-Length: {len(body)}\r\n\r\n".encode() + body


class Client:
    """One LSP client, talking to drey's shim exactly as an editor would."""

    def __init__(self, drey, workspace, config_home):
        env = dict(os.environ)
        env["XDG_CONFIG_HOME"] = config_home
        self.ws = workspace
        self.next_id = 0
        self.proc = subprocess.Popen(
            [drey, "serve", "rust-analyzer"], cwd=workspace, env=env,
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)

    def send(self, method, params, notify=False):
        msg = {"jsonrpc": "2.0", "method": method, "params": params}
        if not notify:
            self.next_id += 1
            msg["id"] = self.next_id
        self.proc.stdin.write(frame(msg))
        self.proc.stdin.flush()
        return self.next_id

    def read(self, timeout):
        end = time.time() + timeout
        header = b""
        while not header.endswith(b"\r\n\r\n"):
            if time.time() > end:
                raise TimeoutError("header")
            ch = self.proc.stdout.read(1)
            if not ch:
                raise EOFError("server closed")
            header += ch
        length = int(next(line for line in header.decode().split("\r\n")
                          if line.lower().startswith("content-length")).split(":")[1])
        body = b""
        while len(body) < length:
            chunk = self.proc.stdout.read(length - len(body))
            if not chunk:
                raise EOFError("truncated body")
            body += chunk
        return json.loads(body)

    def await_response(self, want, timeout=180):
        end = time.time() + timeout
        while time.time() < end:
            msg = self.read(timeout=max(1, end - time.time()))
            if msg.get("id") == want and ("result" in msg or "error" in msg):
                return msg
            # Answer server-initiated requests so initialization can finish.
            if "method" in msg and "id" in msg:
                self.proc.stdin.write(frame({"jsonrpc": "2.0", "id": msg["id"],
                                             "result": None}))
                self.proc.stdin.flush()
        raise TimeoutError(f"no response to id {want}")

    def initialize(self):
        rid = self.send("initialize", {
            "processId": os.getpid(),
            "rootUri": f"file://{self.ws}",
            "workspaceFolders": [{"uri": f"file://{self.ws}", "name": "smoke"}],
            "capabilities": {"textDocument": {"hover": {"contentFormat": ["plaintext"]}}},
        })
        reply = self.await_response(rid)
        self.send("initialized", {}, notify=True)
        return reply

    def did_open(self, text):
        self.send("textDocument/didOpen", {"textDocument": {
            "uri": f"file://{self.ws}/src/main.rs", "languageId": "rust",
            "version": 1, "text": text}}, notify=True)

    def did_change(self, text, version):
        self.send("textDocument/didChange", {
            "textDocument": {"uri": f"file://{self.ws}/src/main.rs", "version": version},
            "contentChanges": [{"text": text}]}, notify=True)

    def hover(self, line, character):
        rid = self.send("textDocument/hover", {
            "textDocument": {"uri": f"file://{self.ws}/src/main.rs"},
            "position": {"line": line, "character": character}})
        contents = (self.await_response(rid).get("result") or {}).get("contents") or {}
        return contents.get("value", "") if isinstance(contents, dict) else str(contents)


def server_pids(server):
    """PIDs of the real server only.

    Matching on the bare name would also catch every `drey serve rust-analyzer`
    shim and report three processes where there is one server and two clients.
    """
    out = subprocess.run(["pgrep", "-f", f"^{server}"], capture_output=True, text=True)
    return set(out.stdout.split())


FAILURES = []


def check(name, ok, detail=""):
    print(f"  {'PASS' if ok else 'FAIL'}  {name}{('  ' + detail) if detail else ''}",
          flush=True)
    if not ok:
        FAILURES.append(name)


def main():
    drey = find_drey()
    server = find_server(sys.argv)
    print(f"drey:   {drey}\nserver: {server}\n")

    ws = tempfile.mkdtemp(prefix="drey-smoke-")
    cfg = tempfile.mkdtemp(prefix="drey-cfg-")
    os.makedirs(f"{ws}/src")
    os.makedirs(f"{cfg}/drey")
    open(f"{ws}/Cargo.toml", "w").write(
        '[package]\nname = "smoke"\nversion = "0.1.0"\nedition = "2021"\n')
    open(f"{ws}/src/main.rs", "w").write(SRC)
    open(f"{cfg}/drey/config.toml", "w").write(
        f'[server.rust-analyzer]\ncommand = "{server}"\n')

    before = server_pids(server)
    a = Client(drey, ws, cfg)
    b = Client(drey, ws, cfg)
    try:
        check("client A initializes", "capabilities" in (a.initialize().get("result") or {}))
        check("client B initializes", "capabilities" in (b.initialize().get("result") or {}))
        a.did_open(SRC)
        b.did_open(SRC)

        time.sleep(3)
        spawned = server_pids(server) - before
        check("two clients, one server process", len(spawned) == 1, f"spawned={len(spawned)}")

        print("  ... waiting for the server to index", flush=True)
        for _ in range(40):
            if "Widget" in a.hover(0, 15):
                break
            time.sleep(3)

        # B edits without saving. A sent nothing and must not see B's text.
        b.did_change(EDITED, 2)
        time.sleep(3)

        check("a divergent edit did not fork a second server",
              len(server_pids(server) - before) == 1)

        # Interleaved twice: if swapping is broken, the second read is where
        # the other client's text leaks through.
        for round_no in (1, 2):
            ha = a.hover(0, 15)
            hb = b.hover(0, 15)
            check(f"round {round_no}: A sees its own u32, never B's u64",
                  "u32" in ha and "u64" not in ha)
            check(f"round {round_no}: B sees its own u64, never A's u32",
                  "u64" in hb and "u32" not in hb)
            # The layout line proves the server recomputed rather than echoed.
            check(f"round {round_no}: layouts differ (4 bytes vs 8)",
                  "size = 4" in ha and "size = 8" in hb)

        check("the file on disk was never modified",
              open(f"{ws}/src/main.rs").read() == SRC)
    finally:
        for client in (a, b):
            try:
                client.proc.terminate()
            except OSError:
                pass
        shutil.rmtree(ws, ignore_errors=True)
        shutil.rmtree(cfg, ignore_errors=True)

    if FAILURES:
        print(f"\n{len(FAILURES)} check(s) failed")
        return 1
    print("\nall checks passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
