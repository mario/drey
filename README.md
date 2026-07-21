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
session, or an editor alongside the agents, and it scales linearly. The same
applies to gopls, tsserver, clangd, pyright and jdtls; Rust is the example
because it is the one with the measurement.

drey makes them share.

## Install

```sh
brew install mario/drey/drey     # macOS, brings its own Rust toolchain
cargo install drey               # anywhere with a Rust toolchain
```

Then put drey in front of every language server on this machine, for every
client at once:

```sh
drey install                     # --dry-run first, if you want to see the plan
```

That writes a wrapper named after each language server into `~/.drey/bin` and
prepends it to `PATH`, so your existing clients pick drey up without being
reconfigured. Open a new shell and check:

```sh
which rust-analyzer              # ~/.drey/bin/rust-analyzer
drey status                      # what is running, and who is attached
```

To point one client at drey by hand instead, name `drey serve rust-analyzer`
wherever that client currently names `rust-analyzer`.

[docs/install.md](docs/install.md) covers configuring servers, watching what is
running, uninstalling, and what to do when something looks wrong.

## How it works

drey is one binary in two roles. The shim looks exactly like a language server
on stdio. The daemon owns a pool of real servers, one per workspace, and
autostarts on first use.

```
  Claude Code ──▶ drey serve ─┐
  Codex       ──▶ drey serve ─┼──▶ daemon ──▶ one rust-analyzer
  Helix       ──▶ drey serve ─┘
```

A client attaches to a running server when the server name and
`initializationOptions` match and its workspace roots are *contained* in the
roots already indexed. Containment rather than equality means opening a single
crate inside a Cargo workspace costs nothing: it attaches to the workspace
already in memory. Git worktrees and branches deliberately stay separate,
because merging them would produce wrong answers, which is worse than using
memory.

Two clients with different unsaved edits to the same file still share one
process. Each document keeps the text everyone agrees on plus whichever client's
edits the server is currently holding, and the daemon swaps that before
answering. Verified against a real rust-analyzer: with one client holding
`pub size: u32` and another holding `u64`, the same process reports
`size = 4, align = 0x4` to the first and `size = 8, align = 0x8` to the second.
That is a type layout recomputed per client, not an echo of the text they sent.

[docs/architecture.md](docs/architecture.md) explains the three ideas the design
rests on, and the parts that were harder than they look.

## Status

Early. It works and it is tested, but it has been exercised against a mock
server far more than against every real one.

The state swapping rests on an assumption worth stating plainly. It needs the
server to process messages in order, which every LSP server must, and to either
snapshot per request (rust-analyzer does) or answer before the next swap
arrives. A server that batches work across messages could in principle answer
against a state that has since moved. If you hit that, lowering the thrash
threshold makes such a client fork instead.

A swap also resends full document text rather than an incremental edit. That is
cheap for source files and would not be for very large generated ones.

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
