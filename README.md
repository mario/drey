# drey

One language server per workspace, shared by every client.

A drey is a squirrel's nest. This one is shared: many editors and coding agents
crawl into a single indexed workspace instead of each digging their own.

## The problem

Every LSP client starts its own language server. Two Claude Code sessions on one
Rust workspace means two rust-analyzer processes, each holding a complete salsa
database:

```
$ footprint -p 36924   # session A
phys_footprint: 3208 MB
$ footprint -p 28845   # session B
phys_footprint: 4225 MB
```

That is 7.4 GB to answer questions about one copy of one codebase. Add a third
session, or an editor alongside the agents, and it scales linearly.

drey makes them share.

## How it works

drey is one binary in two roles.

**The shim** looks exactly like a language server on stdio. Point any client at
`drey serve rust-analyzer` where it currently names `rust-analyzer`, and nothing
else about that client's setup changes.

**The daemon** owns a pool of real language servers, one per workspace, and
autostarts on first use. Clients attach to an existing server whenever they can.

```
  Claude Code ──▶ drey serve ─┐
  Codex       ──▶ drey serve ─┼──▶ daemon ──▶ one rust-analyzer
  Helix       ──▶ drey serve ─┘
```

### Who gets to share

A client attaches to a running server when the server name matches, the
`initializationOptions` match, and the client's workspace roots are *contained*
in the roots the server already indexed. Containment rather than equality means
opening a single crate inside a Cargo workspace costs nothing: it attaches to
the workspace that is already in memory.

Roots are widened to the outermost directory carrying a marker file
(`Cargo.toml`, `go.mod`, `tsconfig.json`, …) before matching, so two clients
that opened different crates land on the same server.

Git worktrees and branches deliberately stay separate. Different trees hold
different code, and merging them would produce wrong answers, which is worse
than using memory.

### Divergent clients still share one server

Every document carries two texts:

- **base**, what all clients agree it says. For agent workloads this is almost
  always what is on disk, and everyone agrees about that by definition.
- **loaded**, what the server is holding right now: base, overlaid with one
  client's unsaved edits.

Each client also keeps a shadow of every document it has open. Wherever a
client's shadow differs from base, that difference is its *overlay*.

While nobody has unsaved edits, every document is clean, all clients are served
at once, and nothing special happens. When a client with an overlay asks a
question, drey pushes that client's state to the server as full-text
`didChange` notifications, then forwards the request. Two clients editing the
same file to different content get correct, private answers **from one process**.

This is affordable because a language server is already an incremental engine.
Replacing one file's text invalidates only the queries that depend on that file;
the rest of the index stands. A swap costs milliseconds of re-analysis rather
than the gigabytes a second server would cost.

Overlays are short-lived by design. A save collapses one: what a client writes
to disk becomes the text everyone agrees on, so `didSave` advances the base and
the overlay disappears. Closing a file or disconnecting releases one too, with
the server restored to base rather than left holding text nobody believes in.
This is why the mechanism suits agents so well: they edit and save within
seconds, so dirty state barely exists.

Two details keep it honest:

- **The common case stays free.** A client that is the only holder of a clean
  document writes straight through, and its own incremental change is forwarded
  untouched. No swap, no full-text resend.
- **Diagnostics are attributed.** Diagnostics for a dirty document go only to
  the client whose state produced them, since they describe text nobody else
  wrote. Clean documents broadcast to every holder as usual.

Forking survives as the pressure valve. A client causing more than 40 swaps in
10 seconds is thrashing, and at that point a private server is genuinely cheaper
than swapping on every request, so drey promotes it to one and replays its
documents. Ordinary editing never comes close.

Index-level copy-on-write is not possible here and drey does not pretend
otherwise: a salsa database lives in one process's heap, `fork(2)` on a
thread-heavy server is unsafe, and salsa snapshots are read-only views of one
revision rather than branches. State switching is the portable answer, and it
works the same way for gopls, clangd and tsserver as it does for rust-analyzer.

### Server-initiated requests

drey routes `workspace/configuration`, `client/registerCapability` and
`workspace/applyEdit` to exactly one attached client (the lowest client id) and
returns its answer.

This matters more than it looks. rust-analyzer pulls its own settings through
`workspace/configuration`. A proxy that drops server-to-client requests silently
runs the server on defaults, which is the opposite of what you want when you
adopted a proxy to control memory.

Diagnostics go only to clients that have the relevant document open. Everything
else is broadcast.

## Install

```sh
./scripts/install.sh
```

That is the whole thing. It:

1. builds drey and installs it to `~/.drey/bin/drey`
2. finds every language server on your machine and records its **absolute**
   path (resolving asdf shims to the real binary)
3. writes `~/.config/drey/config.toml`
4. writes a wrapper per server into `~/.drey/bin`, and prepends that directory
   to `PATH` in **both** `~/.zshenv` and `~/.zshrc`

Both files, deliberately. `.zshenv` is read by every zsh, including the
non-interactive ones a GUI editor or launchd job uses to spawn a server, which
`.zshrc` alone would miss. `.zshrc` is read afterwards for interactive shells,
where version managers like asdf prepend their own shims and would otherwise end
up in front of ours again.

Every file it touches is backed up with a timestamp first, and nothing outside
`~/.drey`, `~/.config/drey` and one marked block in your shell rc is modified.

Open a new shell, then check:

```sh
which rust-analyzer     # ~/.drey/bin/rust-analyzer
drey status             # empty until a client connects
```

Because the swap happens at `PATH`, every client picks it up with no per-client
configuration: Claude Code, Codex, Zed, Helix, nvim, VS Code launched from a
terminal. Claude Code's LSP plugins resolve `rust-analyzer` and friends from
`PATH`, so they need no edits and keep working across plugin updates.

### Why the wrappers are safe

The wrappers are two-line `sh` scripts:

```sh
#!/bin/sh
exec "$HOME/.drey/bin/drey" serve "rust-analyzer" "$@"
```

`config.toml` points at the *absolute* path of the real binary, never at the
name, so a wrapper can never re-enter itself. Client flags are passed straight
through, and clients that invoke a server with different flags get different
processes, since the flags may change its behaviour.

## Use

Nothing to do. Your existing clients now share servers.

If you would rather not touch `PATH`, point a single client at the shim by hand
instead:

```toml
# Helix languages.toml
[language-server.rust-analyzer]
command = "drey"
args = ["serve", "rust-analyzer"]
```

### Watching it

```sh
$ drey status
SERVER               PID  CLIENTS   DOCS  DIRTY  SWAPS   UPTIME  ROOTS
rust-analyzer      41022        3      7      0      0     412s  /Users/you/Projects/platform
```

`DIRTY` counts documents where some client holds unsaved edits, and `SWAPS`
counts state switches. Both sitting at zero means every client is being served
at once with no swapping, which is the normal state. `SWAPS` climbing fast
against a small client count means clients are fighting over the same files.

`drey status --json` for machine-readable output, `drey gc` to release idle
servers now, `drey stop` to shut everything down. The daemon restarts itself the
next time a client connects.

## Uninstall and revert

```sh
./scripts/uninstall.sh
```

It stops the daemon and every server under it, removes the wrappers, takes the
marked block back out of your shell rc (backing the file up first), removes
`~/.config/drey` and the runtime state, and removes the binary. Open a new shell
and `which rust-analyzer` points at the real one again.

Flags for a partial revert:

```sh
./scripts/uninstall.sh --keep-config    # leave config.toml for later
./scripts/uninstall.sh --keep-binary    # keep drey, remove only the wrappers
```

The script is idempotent and safe to run twice, or after a half-finished
install.

### Reverting by hand

If you would rather not run the script, or it failed halfway, these four steps
are the whole of it:

```sh
drey stop                                   # stop the daemon and its servers
rm -rf ~/.drey                              # wrappers and the binary
rm -rf ~/.config/drey ~/Library/Caches/drey # config, socket, log
# then delete the block between "# >>> drey >>>" and "# <<< drey <<<"
# from BOTH ~/.zshenv and ~/.zshrc
```

### Turning it off temporarily

To disable drey for one shell without uninstalling anything:

```sh
export PATH="${PATH#$HOME/.drey/bin:}"
```

To disable it for everything, comment out the `export PATH` line inside the
drey block in your shell rc and open a new shell. Backups of every file the
installer edited sit next to the originals as `<file>.backup-<timestamp>`.

## Troubleshooting

**`which rust-analyzer` still shows the old path.** The rc change only affects
new shells. Open a new one, or `export PATH="$HOME/.drey/bin:$PATH"`. Note that
`~/.local/bin` sits *after* `~/.asdf/shims` in a typical PATH, which is why the
installer uses `~/.drey/bin` at the front instead. Check all three shell modes,
since they read different files:

```sh
zsh -ic 'which rust-analyzer'   # interactive: needs the .zshrc block
zsh -c  'which rust-analyzer'   # non-interactive: needs the .zshenv block
zsh -lc 'which rust-analyzer'   # login
```

**Already-running clients keep their own servers.** A process that started
before the install has the old PATH, and killing its language server only makes
it spawn another unshared one. Restart the client from a new shell to move it
onto drey.

**A client hangs at startup.** Look at the daemon log:

```sh
tail -f ~/Library/Caches/drey/daemon.log     # macOS
tail -f ~/.local/state/drey/daemon.log       # Linux
```

Run with `DREY_LOG=drey=debug` for per-message detail.

**A server is missing after install.** The installer only wraps what it found on
`PATH` at the time. Install the server, then re-run `./scripts/install.sh`; it
is safe to run repeatedly.

**Memory did not drop.** Check `drey status`: if you see several backends for
what you thought was one workspace, the roots differ. Git worktrees are separate
by design. Different `initializationOptions` also split, on purpose.

## Configure

`~/.config/drey/config.toml`. Builtins cover rust-analyzer, gopls, typescript,
pyright, ruff, clangd, zls, lua, elixir and jdtls; anything here overrides them.

```toml
[server.rust-analyzer]
command = "rust-analyzer"
root_markers = ["Cargo.toml", "rust-project.json"]

[server.rust-analyzer.env]
RA_LRU_CAP = "128"

[daemon]
idle_timeout_secs = 1800   # release a server 30 min after its last client
max_backends = 0           # 0 = no cap
```

## Tests

```sh
cargo test            # 150 unit tests + 11 CLI integration tests
python3 tests/e2e.py  # end-to-end against a mock server, over the real shim
```

The unit tests cover framing (malformed, lowercase and missing `Content-Length`,
bodies dribbled in a byte at a time), UTF-16 position maths (astral-plane
characters, surrogate-pair snapping, CRLF, inverted ranges), request-id
rewriting, config merging, and the sharing policy itself. Several are proptests
checking round-trip invariants against an independent reference implementation,
because the inputs that break position maths are not the ones you would think to
write down.

The end-to-end suite drives the actual `drey serve` binary over stdio the way an
editor would, and asserts the things that are easy to get wrong: that two
clients land on one process, that a sub-crate attaches to its workspace, that
server-initiated requests are delivered, that identical content does not fork,
that a divergent edit does *not* fork either, that each divergent client sees
its own text and never the other's, and that swapping repeatedly between them
stays consistent instead of drifting.

## Status

Early. It works and it is tested, but it has been exercised against a mock
server far more than against every real one.

Known limits:

- State swaps assume the server processes messages in order, which every LSP
  server must, and that it either snapshots per request (rust-analyzer does) or
  answers before the next swap arrives. A server that batches work across
  messages could in principle answer against a state that has since moved. If
  you hit that, the thrash threshold can be lowered so such a client forks
  instead.
- A swap resends full document text rather than an incremental edit. That is
  cheap for source files and would not be for very large generated ones.
- `workspace/didChangeWorkspaceFolders` widens a server in place when the new
  roots are already covered, and forks when they are not.
- Server-initiated requests are answered by one client on behalf of all of them.
  If two clients are configured differently for the same workspace, the lowest
  client id decides. Configure differently and they will not share anyway, since
  `initializationOptions` is part of the key.

## Prior art

[lspmux](https://codeberg.org/p2502/lspmux) does the same job and got there
first; [lspmux-rust-analyzer](https://github.com/sunshowers/lspmux-rust-analyzer)
packages it for this exact use case. drey exists because of the two things they
document as out of scope: server-initiated requests are dropped there, and
document divergence between clients is not reconciled.

## Contributing

Bug reports, patches and new server builtins are welcome. Read
[CONTRIBUTING.md](CONTRIBUTING.md) first: it covers the checks CI runs on every
pull request, and what a change to sharing policy needs before it lands.

## Licence

Apache License 2.0. Copyright 2026 Mario Đanić. See [LICENSE](LICENSE) and
[NOTICE](NOTICE).
