# Releasing

Two channels. `cargo install drey` reaches anyone with a Rust toolchain.
`brew install mario/drey/drey` reaches everyone else, and hides the fact that a
Rust toolchain is involved at all, which matters because a Java developer
running jdtls has no reason to have one.

## One-time setup

**crates.io account.** Log in with GitHub, then verify your email address.
Publishing is blocked until the address is verified and the error message does
not make that obvious.

**The repository has to be public** before the first publish. crates.io renders
the README and links `repository`; a 404 on that link is the fastest way to look
abandoned.

```sh
gh repo edit mario/drey --visibility public --accept-visibility-change-consequences
```

## The first publish, from your laptop

Trusted publishing is configured per crate, and a crate has to exist before you
can configure it. So `0.1.0` goes out by hand:

```sh
cargo login                 # token from crates.io/settings/tokens
cargo package --list        # everything here gets uploaded. read it.
cargo publish --dry-run
cargo publish
```

Scope the token to `publish-new` + `publish-update` with an expiry rather than
taking a full-access one. It lands in `~/.cargo/credentials.toml` in plain text,
so treat it like an SSH key.

Both of these are permanent: the name is yours forever once published, and a
version can be yanked but never deleted.

## Every release after that, from CI

Set up trusted publishing once, and no long-lived token ever touches the repo.

1. **On crates.io:** the crate's Settings → Trusted Publishing → Add. Repository
   owner `mario`, repository name `drey`, workflow filename `release.yml`,
   environment `crates-io`.
2. **On GitHub:** Settings → Environments → New environment, named `crates-io`.
   Add yourself as a required reviewer. Now a stray tag cannot publish on its
   own; it waits for a click.

GitHub mints an OIDC token, crates.io exchanges it for one that expires in about
30 minutes and is scoped to this crate alone. That is the whole reason to prefer
it over a stored `CARGO_REGISTRY_TOKEN`: a leaked secret is a standing key,
whereas a leaked exchange token is worthless by dinner.

Then a release is:

```sh
# bump version in Cargo.toml, update CHANGELOG.md, commit
git tag -a v0.2.0 -m "v0.2.0"
git push origin main --follow-tags
```

`.github/workflows/release.yml` re-runs fmt, clippy, the tests and the e2e
suite, checks the tag matches the version in `Cargo.toml` (disagreement there is
the classic way to burn a version number), waits for your approval, and
publishes.

## Homebrew

Skip homebrew-core. It has notability requirements (roughly 75 stars, 30 forks,
30 watchers, plus a maintainer willing to take it on) and a project with none of
those gets closed politely. Run your own tap instead: a public GitHub repo named
`homebrew-drey`.

```sh
gh repo create mario/homebrew-drey --public --description "Homebrew tap for drey"
```

`Formula/drey.rb`:

```ruby
class Drey < Formula
  desc "Sharing proxy for language servers: one server process per workspace"
  homepage "https://github.com/mario/drey"
  url "https://github.com/mario/drey/archive/refs/tags/v0.1.0.tar.gz"
  sha256 "..."  # shasum -a 256 on the downloaded tarball
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

Users get `brew install mario/drey/drey`. Building from source costs them a
couple of minutes of cargo and costs you nothing: no notarisation, no universal
binary, no release tarballs to babysit. `depends_on "rust" => :build` means brew
installs the toolchain itself, so the user never learns drey is written in Rust.

## Prebuilt binaries, later

The gap neither channel covers is a Linux developer with no Rust toolchain: a
Go or TypeScript person who would benefit and has no path in. Close it when
someone actually asks, with [cargo-dist](https://opensource.axo.dev/cargo-dist/),
which generates the whole release pipeline, a `curl | sh` installer, and a
Homebrew formula that downloads instead of compiling.

Worth it at 100 users. Overkill at 5.

## Versioning

Semver, pre-1.0, so breaking changes bump the minor. The surfaces users actually
integrate with are the CLI flags, the config file keys, and the wrapper scripts
`scripts/install.sh` writes onto `PATH`. Changing any of those is a breaking
change even though none of them is a Rust API.
