# Changelog

Notable changes, newest first. Format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions follow semver,
where pre-1.0 means a breaking change bumps the minor.

The surfaces that count as breaking are the CLI flags, the config file keys, and
the wrapper scripts `scripts/install.sh` puts on your `PATH`. None of them is a
Rust API, and all of them are things people have wired into an editor config.

## [Unreleased]

### Added

- Apache License 2.0, replacing the proprietary licence.
- `CONTRIBUTING.md`, `CODE_OF_CONDUCT.md`, `SECURITY.md`, and a `NOTICE` file.
- `docs/architecture.md` explaining the three ideas the design rests on, and
  `docs/releasing.md` covering crates.io and the Homebrew tap.
- CI on Linux and macOS: fmt, clippy with warnings as errors, unit tests, the
  Python end-to-end suite, an MSRV 1.85 check, and `cargo deny` for licences and
  advisories.
- Release workflow publishing to crates.io from a tag push, via trusted
  publishing rather than a stored token.
- Property tests (proptest) over frame round-trips, UTF-16 position maths, and
  incremental edit application against a reference implementation.

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
