# Changelog

Notable changes, newest first. Format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions follow semver,
where pre-1.0 means a breaking change bumps the minor.

The surfaces that count as breaking are the CLI flags, the config file keys, and
the wrapper scripts `drey install` puts on your `PATH`. None of them is a Rust
API, and all of them are things people have wired into an editor config.

## [Unreleased]

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
