# Releasing

A release is a tag push. CI does the rest.

```sh
# bump version in Cargo.toml, update CHANGELOG.md, commit
git tag -a v0.1.0 -m "v0.1.0"
git push origin main --follow-tags
```

`.github/workflows/release.yml` then runs, in order:

1. `cargo fmt --all -- --check`, `cargo clippy --all-targets -- -D warnings`,
   `cargo test`, and `cargo build && python3 tests/e2e.py`. A tag push skips the
   pull-request run, so the gate runs again here.
2. A check that the tag matches `version` in `Cargo.toml`. They disagree more
   often than you would expect, and crates.io has no undo.
3. A pause on the `crates-io` GitHub environment, which has a required
   reviewer. Nothing publishes until someone approves the deployment.
4. `cargo publish`, using a token obtained through trusted publishing.

No `CARGO_REGISTRY_TOKEN` secret is stored in the repository. GitHub mints an
OIDC identity token for the job, `rust-lang/crates-io-auth-action` exchanges it
for a crates.io token valid for about 30 minutes and scoped to this crate, and
that token is discarded when the job ends.

## One-time setup

**Repository visibility.** crates.io renders the README and links `repository`,
so the repository has to be public before the first publish.

```sh
gh repo edit mario/drey --visibility public --accept-visibility-change-consequences
```

**crates.io account.** Log in with GitHub and verify your email address.
Publishing is rejected until the address is verified.

**Trusted publishing.** On crates.io, under the crate's Settings → Trusted
Publishing → Add:

| Field | Value |
| --- | --- |
| Repository owner | `mario` |
| Repository name | `drey` |
| Workflow filename | `release.yml` |
| Environment | `crates-io` |

**The GitHub environment.** Settings → Environments → New environment, named
`crates-io`, with yourself as a required reviewer. This is what makes step 3
above a gate rather than a formality.

If crates.io requires the crate to exist before it will accept a trusted
publishing config, bootstrap it with one local `cargo publish` using a token
scoped to `publish-new`, then configure trusted publishing and delete the token.
Every release after that goes through CI.

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
