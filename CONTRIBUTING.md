# Contributing to drey

drey sits between your editor and a language server, in the hot path of every
completion you type. A bug here does not look like a bug in drey. It looks like
rust-analyzer went quiet, or hover started lying, and you lose an afternoon
before you suspect the proxy. That is the whole reason the bar below exists.

Patches, bug reports and new server builtins are all welcome.

## Before you write code

For a bug fix, open an issue or just send the patch. For anything that changes
*which clients share a server*, open an issue first. Sharing policy is the part
where a wrong answer is silent: two clients merge when they should not have, and
the second one gets completions computed against the first one's file contents.
I would rather argue about that in an issue than in a revert.

## The local loop

```sh
cargo fmt --all                             # format
cargo clippy --all-targets -- -D warnings   # lint, warnings are errors
cargo test                                  # unit + integration
cargo build && python3 tests/e2e.py         # real shim, mock server, over stdio
```

CI runs exactly these on Linux and macOS, plus `cargo fmt --all -- --check` and
`cargo deny check` for licences and advisories. Run them locally and CI holds no
surprises.

The end-to-end suite needs a debug build first, because it locates and drives the
actual `drey serve` binary the way an editor would.

## Rust standards

The rules that are mechanical are enforced by `rustfmt.toml` and the `[lints]`
block in `Cargo.toml`. Those you do not need to remember. The rest:

**Prefer `?` over `unwrap()` and `expect()` in `src/`.** A panic in the shim
takes down the client's language support with no error message anyone can read.
Return `anyhow::Result` and let the caller decide.

This one is a review rule, not a lint, and I should be straight about why: there
are 29 existing call sites in the binary today. Turning on
`clippy::unwrap_used` would fail the build on all of them, and rewriting them in
a hurry is how you turn a clean panic into a wrong answer. New code should not
add to the pile, and if you are already in a function that panics on a case that
can actually happen, fixing it is welcome. Tests may unwrap freely: a panicking
test is a failing test, which is the point.

**No `unsafe`.** The crate is `#![forbid(unsafe_code)]`. If you find a case that
genuinely needs it, that is an issue, not a pull request.

**Errors carry context.** `.context("reading the initialize response")?` beats a
bare `?`. When a user pastes `RUST_LOG=drey=debug` output into an issue, the
error chain is the whole story.

**Nothing blocking on an async task.** Everything runs on tokio. A blocking
read, a `std::fs` call on a slow path, a `std::sync::Mutex` held across an
`.await`: any of these stall every client on that runtime thread, not just the
one that caused it. Use `tokio::fs`, `tokio::sync`, or `spawn_blocking`.

**Types over comments where a type will do.** A `ClientId(u64)` cannot be passed
where a `RequestId` belongs. A comment saying "this is a client id" can be
ignored by anyone in a hurry, including you in six months.

**Comment the why, not the what.** The existing code is sparse on comments and
that is deliberate. When you do write one, write the thing the code cannot say:
the LSP spec paragraph you are working around, the editor that sends malformed
`didChange`, the reason two obvious approaches were wrong.

**Public items get doc comments.** `///` on anything `pub`, with the invariants
the caller has to hold up.

## Tests

New behaviour ships with tests. That is not negotiable, but it is also not a
coverage-percentage game. The question a reviewer asks is: *if I reverted your
change, would a test fail?* If the answer is no, the test is decoration.

Three kinds live here, and the right one depends on what you touched:

- **Unit tests** next to the code, for pure logic: framing, UTF-16 position
  maths, config merging, root widening. Fast, and they pin the edge cases
  (empty document, astral-plane character, CRLF, split read).
- **Property tests** (proptest) for round-trip invariants. Encoding a frame and
  decoding it returns the input; a byte offset converted to an LSP position and
  back is unchanged; a sequence of incremental edits matches a naive reference
  implementation. These catch the inputs you would never think to write down.
- **End-to-end** in `tests/e2e.py`, driving the real binary over stdio against a
  mock server. Anything about sharing, forking, divergence or server-initiated
  requests belongs here, because those are properties of two processes talking,
  not of a function.

A change to sharing policy needs an e2e test that fails without it. This is the
one rule I will nitpick in review.

Name tests for the behaviour they pin, not the function they call.
`sub_crate_attaches_to_workspace_server` tells a future reader what broke.
`test_roots_2` does not.

## Adding a server builtin

Builtins live in `src/config.rs`. A new one needs the command, the root markers,
and an e2e or unit test showing the markers widen roots the way you expect. Say
in the pull request which version of the server you tested against.

## Commits and pull requests

Present tense, lower case, explain the change not the diff: `widen roots before
matching so sub-crates attach`. If the commit needs a paragraph, write the
paragraph. Rebase on `main` rather than merging.

The pull request should say what breaks if the change is wrong. That framing
finds more problems in review than a description of the implementation does.

## Reporting bugs

Include the client (editor or agent, and version), the language server and
version, and `RUST_LOG=drey=debug drey serve <server>` output around the failure.
If clients shared when they should not have, the roots each one sent are the
first thing anyone will ask for.

Security issues: do not open a public issue. See [SECURITY.md](SECURITY.md).

## Licence

Contributions are licensed under Apache License 2.0, per section 5 of the
licence. No CLA, no copyright assignment. You keep your copyright.
