# Changelog

Notable changes, newest first. Format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions follow semver,
where pre-1.0 means a breaking change bumps the minor.

The surfaces that count as breaking are the CLI flags, the config file keys, and
the wrapper scripts `drey install` puts on your `PATH`. None of them is a Rust
API, and all of them are things people have wired into an editor config.

## [Unreleased]

## [0.1.8]

### Fixed

- `serve` now gives up on a client that never writes anything, instead of
  parking on stdin forever. The daemon side already bounded the reverse
  handshake (0.1.5's `INIT_TIMEOUT`: a backend that doesn't answer
  `initialize` within 60 seconds is given up on), but nothing bounded a
  client that opens the shim's stdin and never sends its own `initialize`.
  That left an orphaned `drey serve` process per abandoned attempt, sitting
  outside `drey status` (which lists backends, not shims) and outside process
  accounting until something went looking. A client that writes nothing
  within 60 seconds now gets the same treatment: the shim exits instead of
  hanging.

## [0.1.7]

### Fixed

- The guard against a server config pointing back at drey itself now checks
  the exact executable path instead of banning its whole parent directory.
  Homebrew installs `drey` at `/usr/local/bin/drey`, the same directory as
  `asdf` and every other server binary it manages; `drey install` correctly
  points asdf-managed servers at `/usr/local/bin/asdf`
  (`command = "/usr/local/bin/asdf"`, `args = ["exec", "rust-analyzer"]`), but
  the guard read that as drey pointing at itself and refused to spawn it. Six
  of the twelve built-in servers route through asdf, so this hung silently for
  anyone with drey installed next to a version manager in the same directory,
  the same "attached to nothing, no error" symptom as 0.1.5's shim loop.

## [0.1.6]

### Fixed

- Wrapper scripts written by `drey install` now keep the path they were
  invoked through instead of resolving it. Homebrew runs `drey` via
  `/usr/local/bin/drey`, a symlink it repoints at every upgrade to point at
  the newly installed version's Cellar path; canonicalizing that symlink baked
  the old, versioned Cellar path into every wrapper. The next upgrade deleted
  that path, so every wrapper drey had installed, not just rust-analyzer's,
  called a binary that no longer existed, and the daemon that `serve` is
  supposed to autostart never got to run.

## [0.1.5]

### Fixed

- The shim directory of the version manager drey launches through is now
  removed from the server's environment, alongside drey's own. Launching
  rust-analyzer as `asdf exec rust-analyzer` reaches a rustup proxy inside the
  asdf installation; when that toolchain has no `rust-analyzer` component, the
  proxy falls back to the next `rust-analyzer` on `PATH`, which is asdf's shim,
  which execs `asdf exec rust-analyzer` again. The two proxies handed the name
  back and forth and never reached a language server. What you saw was a client
  attached to nothing: no diagnostics, no completion, no error, and a `drey
  serve` process that sat there for days. With the shims gone from the child's
  `PATH`, rustup says which component is missing instead.

- A server that does not answer `initialize` within 60 seconds is now given up
  on, with its stderr attached. Every real server replies in well under a
  second and indexes afterwards, so a slow handshake is a wedged one. There was
  no bound before, so a single stuck spawn kept every client on that workspace
  waiting for as long as the daemon lived.

## [0.1.4]

### Fixed

- Servers managed by asdf are launched through `asdf exec` again, instead of
  the path `asdf which` reports. That path is often not a program: for
  asdf-managed Rust it is a rustup proxy that needs `RUSTUP_HOME` pointing
  inside the asdf installation, and only `asdf exec` sets it. Unwrapped, the
  proxy searched `~/.rustup`, found no default toolchain there, and exited
  during initialize, so every Rust client silently had no language server at
  all. gopls, ruff, pyright and ruby-lsp resolve through the same shims and
  were affected wherever their runtime needed the environment too.

  Unwrapping was meant to stop every workspace from pinning to whatever
  version asdf picked at daemon start. Recording `asdf exec` fixes that
  properly rather than sidestepping it: the daemon already spawns servers with
  the workspace root as the working directory, which is what asdf dispatches
  on, so each workspace now gets the version its `.tool-versions` asks for.

- A server that dies during initialize now reports what it printed on stderr.
  That output went to the log at debug level, so the usual causes (a missing
  toolchain, an unknown flag, a broken shim) produced `exited during
  initialize` and nothing else. Diagnosing the bug above meant turning up
  `DREY_LOG` and reproducing it; the message now carries the reason.

## [0.1.3]

### Fixed

- `Content-Length` is matched without regard to case, as header names are. Two
  spellings were accepted and `CONTENT-LENGTH:` was not, so a client using it
  got "LSP message without Content-Length header", a confusing way to report
  unexpected capitalisation. A field whose name merely ends in `content-length`
  is still ignored rather than parsed as the length.
- The test suites leaked daemons. A daemon detaches and only `drey stop` ends
  it, and the CLI suite never stopped what it started, so 8 survived every run.
  Dozens accumulated here over a day, the oldest at 18 hours, and the suite
  began timing out under the process pressure. The real-server smoke test
  leaked a daemon and a rust-analyzer too. Both now clean up.

## [0.1.2]

### Fixed

- Root widening could climb to `$HOME`. A stray `Cargo.toml`, `package.json` or
  `go.mod` in your home directory merged every project underneath it into one
  backend indexing the entire home directory. The walk now stops below home in
  both directions: it will not adopt home as a root, and a client that opened
  `$HOME` itself will not climb into `/Users` or `/`.
- A duplicate `Content-Length` header let the last value win, which
  desynchronised the stream: the sender and drey then disagreed about where the
  message ended, and every message after it was read at the wrong offset with no
  error to point at the cause. It is now refused, naming both values.

### Changed

- Hitting the 64-entry cap on server-initiated requests held for an absent
  client now logs at warn rather than passing silently. A server whose requests
  are being refused is wedged or misconfigured, and the log was the only place
  that would have shown it.

### Note for anyone upgrading

If you had a marker file directly in `$HOME`, projects that previously collapsed
into a single backend now get one each. That is more memory and more indexing on
first use, and it is the correct behaviour: those projects were never one
workspace. Nothing else changes, and no configuration is affected.

## [0.1.1]

### Added

- `drey install` and `drey uninstall`, so a binary from Homebrew or
  `cargo install` can set up the `PATH` interposition without cloning the
  repository and rebuilding. The wrappers exec whichever drey binary you ran
  `install` from, rather than assuming `~/.drey/bin/drey`. Both take
  `--dry-run`; `install` takes `--force` to replace files in `~/.drey/bin` that
  drey did not write.
- `tests/smoke_real.py`, a smoke test against a real rust-analyzer rather than
  the mock. Two clients hold different unsaved versions of one file and both
  ask about it; the same process reports a 4 byte layout to one and 8 bytes to
  the other. Not run in CI, since it needs a real server and time to index.

### Fixed

- The generated config was never read on macOS. `scripts/install.sh` wrote
  `~/.config/drey/config.toml`, but `config_path()` uses `dirs::config_dir()`,
  which is `~/Library/Application Support/drey` there. drey fell back to
  builtins with bare command names and worked anyway, because the recursion
  guard strips the wrapper directory from the child's environment and the bare
  name then resolves to the real binary. `drey install` uses `config_path()`,
  and the docs now name both platforms.
- A race in `a_save_makes_one_clients_text_the_shared_truth`, which failed on
  Linux in CI and reproduced once in ten locally. The test never waited for the
  second client's `didOpen` to be processed, so it could land after the save and
  leave the document dirty forever.

### Changed

- `scripts/install.sh` and `scripts/uninstall.sh` are thin wrappers now: they
  build or remove the binary and delegate the rest to the new subcommands. The
  discovery, config, wrapper and `PATH` logic is no longer duplicated in shell.
- `drey uninstall` leaves the config file in place and says so, where the old
  script deleted it. It also removes only files carrying drey's marker comment
  from `~/.drey/bin`.

## [0.1.0]

First working version.

- The shim: a process that looks like a language server on stdio and forwards to
  the daemon.
- The daemon: one real language server per workspace, autostarted, released
  after an idle timeout.
- Sharing by root containment, so opening one crate inside a Cargo workspace
  attaches to the server already holding it.
- State switching, so two clients with different unsaved edits to the same file
  share one process instead of forking a second.
- Server-initiated requests answered by one attached client on behalf of all.
- Recursion guards, so wrapper scripts on `PATH` cannot make the daemon spawn
  itself, including through version-manager shims at absolute paths.
- Builtins for rust-analyzer, gopls, typescript, pyright, ruff, clangd, zls,
  lua, elixir and jdtls.
- Apache License 2.0, contributor and security docs, and `docs/architecture.md`.
- CI on Linux and macOS: fmt, clippy with warnings as errors, unit tests, the
  Python end-to-end suite, an MSRV 1.85 check, and `cargo deny` for licences and
  advisories. Release from a tag push via crates.io trusted publishing.
- 150 unit tests and 11 CLI integration tests, including proptests over frame
  round-trips, UTF-16 position maths, and incremental edit application against a
  reference implementation.
