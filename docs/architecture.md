# Architecture

The README explains what drey does. This explains how, for anyone about to
change it.

One binary, two roles. `drey serve rust-analyzer` is the **shim**: it looks like
a language server on stdio and forwards to a Unix socket. `drey daemon` owns the
**backends**, one real language server process per workspace, and autostarts the
first time a shim cannot find it.

```
  client stdio ──▶ shim ──socket──▶ daemon ──▶ Session (per client)
                                       │            │
                                       │            └─ shadow docs
                                       └─ Registry ──▶ Backend ──▶ rust-analyzer
                                                          └─ shared docs
```

## The modules

| File | Job |
| --- | --- |
| `main.rs` | CLI: `serve`, `daemon`, `status`, `stop`. |
| `shim.rs` | Client-facing half. Connects (autostarting the daemon), sends `Hello`, splices stdio to the socket. |
| `framing.rs` | LSP base protocol: `Content-Length` headers over a byte stream. |
| `msg.rs` | JSON-RPC helpers, and the request-id namespacing described below. |
| `text.rs` | Document text plus UTF-16 position maths. |
| `config.rs` | Builtins for ten servers, overridden by the user config. `config_path()` is platform-dependent; `DREY_CONFIG` overrides it. |
| `daemon/mod.rs` | Socket, connection accept loop, root extraction and widening. |
| `daemon/registry.rs` | The backend pool, and the policy for who shares what. |
| `daemon/backend.rs` | One real server process, its `BackendKey`, and the shared documents. |
| `daemon/session.rs` | One connected client and its copy-on-write view of those documents. |

## Three ideas hold the whole thing up

**Sharing by containment, not equality.** A `BackendKey` is the server name, the
workspace roots, and `initializationOptions`. A client attaches when an existing
key `covers` its own: same name and options, and its roots contained in the
ones already indexed. Roots are widened first, out to the outermost directory
carrying a marker file (`Cargo.toml`, `go.mod`, `tsconfig.json`), so opening one
crate inside a Cargo workspace attaches to the workspace already in memory
instead of indexing it again.

Git worktrees and branches deliberately do not share. Different trees hold
different code, and merging them returns wrong answers, which costs more than
the memory saves.

**One id namespace.** Every client numbers its requests from 1, so two clients
collide immediately. `msg.rs` rewrites ids into a single server-facing namespace
by prefixing the client id, and undoes it on the response. The separator is a
control character, chosen so it cannot collide with a string id any real client
would send. Requests drey issues itself carry their own prefix, which is how the
daemon recognises responses meant for it rather than for a client.

**Copy-on-write documents.** A `SharedDoc` holds two versions: `base`, the text
every client agrees on (usually what is on disk), and `loaded`, what the server
is holding right now. When two clients have the same file open with different
unsaved edits, drey does not fork a second server. It swaps `loaded` to the
right client's text before serving that client's request, via a
`didChange`/`didOpen` sequence the server treats as an ordinary edit. Each
`Session` keeps its own shadow, so a client always sees its own text and never
the other's.

In full: every document carries two texts. `base` is what all clients agree it
says, which for agent workloads is almost always what is on disk. `loaded` is
what the server holds right now, meaning base overlaid with one client's unsaved
edits. Each `Session` also keeps a shadow of every document it has open, and
wherever a shadow differs from base, that difference is that client's overlay.

While nobody has unsaved edits every document is clean, all clients are served
at once, and nothing special happens. When a client with an overlay asks a
question, drey pushes that client's state to the server as full-text
`didChange` notifications and then forwards the request.

This is affordable because a language server is already an incremental engine.
Replacing one file's text invalidates only the queries that depend on that file;
the rest of the index stands. A swap costs milliseconds of re-analysis rather
than the gigabytes a second server would cost.

Overlays are short-lived by design. A save collapses one: what a client writes
to disk becomes the text everyone agrees on, so `didSave` advances base and the
overlay disappears. Closing a file or disconnecting releases one too, with the
server restored to base rather than left holding text nobody believes in. This
is why the mechanism suits agents: they edit and save within seconds, so dirty
state barely exists.

Two details keep it honest. The common case stays free, since a client that is
the only holder of a clean document writes straight through with its own
incremental change forwarded untouched. And diagnostics are attributed, so those
for a dirty document go only to the client whose state produced them, because
they describe text nobody else wrote. Clean documents broadcast to every holder.

Swapping is not free. A client causing more than 40 swaps in 10 seconds is
thrashing, and at that point a private server is genuinely cheaper than swapping
on every request, so drey promotes it to one and replays its documents. Ordinary
editing never comes close. The constant and its reasoning are in `session.rs`.

Index-level copy-on-write is not possible here and drey does not pretend
otherwise: a salsa database lives in one process's heap, `fork(2)` on a
thread-heavy server is unsafe, and salsa snapshots are read-only views of one
revision rather than branches. State switching is the portable answer, and it
works the same way for gopls, clangd and tsserver as it does for rust-analyzer.

## Things that were harder than they look

**Recursion.** The install script puts wrapper scripts named `rust-analyzer` on
your `PATH`, so every client picks up drey without reconfiguration. That means
the daemon spawning `rust-analyzer` would spawn drey. Stripping the wrapper
directory from the child's environment is the fix, and it has to be the
directory rather than the binary name: version managers install proxies at
absolute paths that look the real binary up on `PATH` again. Configuring an
absolute path is not enough. See the comment above `spawn_path` in `backend.rs`.

**Server-initiated requests.** The server asks questions too
(`workspace/configuration`, `window/showMessage`, registration). With N clients
attached there is no obvious one to ask. The lowest client id answers on behalf
of all of them. Clients configured differently would not be sharing in the first
place, since `initializationOptions` is part of the key.

**Workspaces that change mid-session.** `workspace/didChangeWorkspaceFolders`
widens a server in place when the new roots are already covered, and forks when
they are not. Widening in place is the whole point of containment matching, and
forking is the only honest answer when a client asks for something the running
server never indexed.

**The socket is an execution surface.** Anyone who can write to it can make the
daemon spawn a configured server. It is kept to the owning user, and that is a
property worth not regressing.

## Where to add things

A new language server is usually a builtin in `config.rs`: command, root
markers, done. A change to who shares what is `BackendKey::covers` and
`Registry`, and it needs an end-to-end test that fails without it. Anything
touching document state runs through `SharedDoc` and `Session`, which is the
part where a bug is silent and shows up as a stale completion three files away.
