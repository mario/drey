# Releasing

A release is a tag push. CI does the rest.

```sh
# bump version in Cargo.toml, update CHANGELOG.md, commit
git tag -a v0.1.0 -m "v0.1.0"
git push origin main --follow-tags
```

`.github/workflows/release.yml` then runs, in order:

1. **verify**: `cargo fmt --all -- --check`, `cargo clippy --all-targets -- -D
   warnings`, `cargo test`, and `cargo build && python3 tests/e2e.py`. A tag
   push skips the pull-request run, so the gate runs again here. It also checks
   the tag against `version` in `Cargo.toml`; they disagree more often than you
   would expect, and crates.io has no undo.
2. **github_release**: creates the release from the matching `CHANGELOG.md`
   section, falling back to generated notes. It does not wait for approval, so
   the releases page is populated as soon as the tag exists.
3. **publish**: pauses on the `crates-io` GitHub environment, which has a
   required reviewer, then runs `cargo publish`.
4. **homebrew**: only after publish succeeds, points the tap's formula at the
   new tag and commits the recomputed checksum.

No `CARGO_REGISTRY_TOKEN` is stored anywhere. GitHub mints an OIDC identity
token for the job, `rust-lang/crates-io-auth-action` exchanges it for a
crates.io token valid for about 30 minutes and scoped to this crate, and that
token dies with the job.

Publishing with an API token is disabled on the crates.io side, so this workflow
is the only way a version reaches the registry. A leaked personal token cannot
publish drey, and neither can a maintainer in a hurry on a laptop.

## One-time setup

Already done for this repository. Recorded because the next project needs it,
and because someone will eventually ask why the tap has a deploy key.

**Trusted publishing**, on crates.io under the crate's Settings, Trusted
Publishing:

| Field | Value |
| --- | --- |
| Repository owner | `mario` |
| Repository name | `drey` |
| Workflow filename | `release.yml` (the filename, not a path) |
| Environment | `crates-io` |

Trusted publishing is configured per crate, and a crate has to exist before you
can attach one, so `0.1.0` was published by hand with a scoped token. That token
is gone and API-token publishing is now disabled on the crate, which means this
workflow is the only path to the registry.

**The GitHub environment**: Settings, Environments, named `crates-io`, with a
required reviewer and a deployment branch policy limiting it to `v*` tags.

**The tap deploy key**: an SSH key with write access to `mario/homebrew-drey`
and nothing else, stored here as `HOMEBREW_TAP_DEPLOY_KEY`. `GITHUB_TOKEN`
cannot reach another repository, and a personal access token would carry write
access to everything the account can see in order to push one file.


## Version numbers

Semver. Pre-1.0, a breaking change bumps the minor.

The surfaces users integrate with are the CLI flags, the config file keys in
`~/.config/drey/config.toml`, and the wrapper scripts `scripts/install.sh`
writes onto `PATH`. A change to any of those is breaking, even though none of
them is a Rust API.

A published version can be yanked but never deleted, and the crate name is
permanent after the first publish.

## Homebrew

The tap is a separate public repository, `mario/homebrew-drey`, containing
`Formula/drey.rb`. Users install with:

```sh
brew install mario/drey/drey
```

The formula builds from source and declares `depends_on "rust" => :build`, so
Homebrew provides the toolchain. Someone running jdtls or gopls never has to
install Rust themselves.

```ruby
class Drey < Formula
  desc "Sharing proxy for language servers: one server process per workspace"
  homepage "https://github.com/mario/drey"
  url "https://github.com/mario/drey/archive/refs/tags/v0.1.0.tar.gz"
  sha256 "..."  # shasum -a 256 on the release tarball
  license "Apache-2.0"
  head "https://github.com/mario/drey.git", branch: "main"

  depends_on "rust" => :build

  def install
    system "cargo", "install", *std_cargo_args
  end

  test do
    assert_match "drey", shell_output("#{bin}/drey --help")
  end
end
```

Updating the formula after a release means changing `url` to the new tag and
`sha256` to the new tarball's checksum:

```sh
curl -sL https://github.com/mario/drey/archive/refs/tags/v0.1.0.tar.gz | shasum -a 256
```

Homebrew's own repository, homebrew-core, has notability requirements the
project does not currently meet.

## Prebuilt binaries

Not currently published. A user without a Rust toolchain, on a platform
Homebrew does not cover, has no install path today.
[cargo-dist](https://opensource.axo.dev/cargo-dist/) generates the release
pipeline, a shell installer, and a Homebrew formula that downloads rather than
compiles, when that becomes worth maintaining.

## Reproducing CI locally

```sh
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo build && python3 tests/e2e.py
```

Or in a container matching the CI environment, with every base image pinned by
digest:

```sh
docker build --target verify .   # the full gate
docker build --target msrv .     # MSRV 1.85 check
```
