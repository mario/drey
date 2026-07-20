#!/usr/bin/env python3
"""A minimal LSP server used by the end-to-end test.

It records how many times it was started (by appending to $MOCK_TALLY), answers
`initialize` and a custom `test/ping`, and issues a server-to-client
`workspace/configuration` request so the test can prove that direction works.
"""
import json
import os
import sys
import threading

def read_message(stream):
    length = None
    while True:
        line = stream.readline()
        if not line:
            return None
        line = line.decode("utf-8").strip()
        if line == "":
            break
        if line.lower().startswith("content-length:"):
            length = int(line.split(":", 1)[1].strip())
    if length is None:
        return None
    return json.loads(stream.read(length).decode("utf-8"))

def apply_change(text, change):
    """Applies one contentChanges entry, with UTF-16 position semantics."""
    if "range" not in change:
        return change["text"]

    def offset(pos):
        line, character = pos["line"], pos["character"]
        idx = 0
        for _ in range(line):
            nxt = text.find("\n", idx)
            if nxt == -1:
                return len(text)
            idx = nxt + 1
        units = 0
        i = idx
        while i < len(text) and text[i] != "\n" and units < character:
            units += 2 if ord(text[i]) > 0xFFFF else 1
            i += 1
        return i

    start = offset(change["range"]["start"])
    end = max(start, offset(change["range"]["end"]))
    return text[:start] + change["text"] + text[end:]

lock = threading.Lock()

def write_message(payload):
    body = json.dumps(payload).encode("utf-8")
    with lock:
        sys.stdout.buffer.write(b"Content-Length: %d\r\n\r\n" % len(body))
        sys.stdout.buffer.write(body)
        sys.stdout.buffer.flush()

def main():
    tally = os.environ.get("MOCK_TALLY")
    if tally:
        with open(tally, "a") as fh:
            fh.write("start %d\n" % os.getpid())

    open_docs = {}

    while True:
        msg = read_message(sys.stdin.buffer)
        if msg is None:
            return
        method = msg.get("method")

        if method == "initialize":
            write_message({
                "jsonrpc": "2.0", "id": msg["id"],
                "result": {"capabilities": {"textDocumentSync": 2}, "serverInfo": {"name": "mock"}},
            })
        elif method == "initialized":
            # Pull settings from the client. Other multiplexers drop this.
            write_message({
                "jsonrpc": "2.0", "id": "cfg-1", "method": "workspace/configuration",
                "params": {"items": [{"section": "mock"}]},
            })
        elif method == "textDocument/didOpen":
            doc = msg["params"]["textDocument"]
            open_docs[doc["uri"]] = doc["text"]
        elif method == "textDocument/didChange":
            uri = msg["params"]["textDocument"]["uri"]
            for change in msg["params"]["contentChanges"]:
                open_docs[uri] = apply_change(open_docs.get(uri, ""), change)
        elif method == "textDocument/didClose":
            open_docs.pop(msg["params"]["textDocument"]["uri"], None)
        elif method == "test/ping":
            # Report our identity and what we believe is open, so the test can
            # tell which process answered and whether state is shared.
            write_message({
                "jsonrpc": "2.0", "id": msg["id"],
                "result": {"pid": os.getpid(), "open": sorted(open_docs.keys()),
                           "texts": open_docs},
            })
        elif method == "test/diagnose":
            # Publish diagnostics for a document on demand, so tests can check
            # who they are delivered to.
            write_message({
                "jsonrpc": "2.0", "method": "textDocument/publishDiagnostics",
                "params": {"uri": msg["params"]["uri"], "diagnostics": []},
            })
            write_message({"jsonrpc": "2.0", "id": msg["id"], "result": None})
        elif method == "shutdown":
            write_message({"jsonrpc": "2.0", "id": msg["id"], "result": None})
        elif method == "exit":
            return

if __name__ == "__main__":
    main()
