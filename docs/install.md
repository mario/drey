# Installing, configuring and removing drey

The README covers the one-line install. This is everything after that.

## What `drey install` does

It finds every language server on your machine and records its absolute path,
resolving asdf shims to the real binary, writes those paths into the config, and
puts a wrapper per server into `~/.drey/bin`. Each wrapper execs the drey binary
you ran `install` from, so a Homebrew copy stays a Homebrew copy. Then it
prepends that directory to `PATH` in both `~/.zshenv` and `~/.zshrc`.

Both files, deliberately. `.zshenv` is read by every zsh, including the
non-interactive ones a GUI editor or launchd job uses to spawn a server, which
`.zshrc` alone would miss. `.zshrc` is read afterwards for interactive shells,
where version managers like asdf prepend their own shims and would otherwise end
up in front of ours again.

Every file it touches is backed up with a timestamp first, and nothing outside
`~/.drey`, the config directory and one marked block in your shell rc is
modified. Running it again is safe: install a new language server, re-run
`drey install`, and it picks the new one up.

From source, `./scripts/install.sh` builds drey into `~/.drey/bin` first and
then runs the same `drey install`:

```sh
git clone https://github.com/mario/drey && cd drey
./scripts/install.sh
```

### Why the wrappers are safe

The wrappers are short `sh` scripts:

```sh
#!/bin/sh
# Installed by drey. Routes this language server through the shared daemon.
exec "/usr/local/bin/drey" serve "rust-analyzer" "$@"
```

The config points at the *absolute* path of the real binary, never at the name,
so a wrapper can never re-enter itself. Client flags are passed straight
through, and clients that invoke a server with different flags get different
processes, since the flags may change its behaviour.

Because the swap happens at `PATH`, every client picks it up with no per-client
configuration: Claude Code, Codex, Zed, Helix, nvim, VS Code launched from a
terminal. Claude Code's LSP plugins resolve `rust-analyzer` and friends from
`PATH`, so they need no edits and keep working across plugin updates.

## Pointing a single client at drey

If you would rather not touch `PATH`:

```toml
# Helix languages.toml
[language-server.rust-analyzer]
command = "drey"
args = ["serve", "rust-analyzer"]
```

## Watching it

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

## Configure

`drey install` writes the config for you. To edit it by hand, the path is
`~/Library/Application Support/drey/config.toml` on macOS and
`~/.config/drey/config.toml` on Linux, or wherever `DREY_CONFIG` points if you
set it. Builtins cover rust-analyzer, gopls, typescript, pyright, ruff, clangd,
zls, lua, elixir and jdtls; anything in the file overrides them.

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

## Uninstall and revert

```sh
drey uninstall          # --dry-run to see what it would touch
```

It removes the wrappers, only files carrying drey's marker comment and never
anything else in `~/.drey/bin`, and takes the marked block back out of your
shell rc, backing the file up first. The config file is left in place, since it
is the only thing that took effort to produce; delete it yourself if you want it
gone. Open a new shell and `which rust-analyzer` points at the real one again.

If you installed from source, `./scripts/uninstall.sh` does the same and also
stops the daemon and removes the binary:

```sh
./scripts/uninstall.sh
./scripts/uninstall.sh --keep-binary    # keep drey, remove only the wrappers
```

Both are idempotent and safe to run twice, or after a half-finished install.

### Reverting by hand

If the script failed halfway, these four steps are the whole of it:

```sh
drey stop                                    # stop the daemon and its servers
rm -rf ~/.drey                               # wrappers and the binary
rm -rf ~/Library/Application\ Support/drey \
       ~/.config/drey ~/Library/Caches/drey  # config, socket, log
# then delete the block between "# >>> drey >>>" and "# <<< drey <<<"
# from BOTH ~/.zshenv and ~/.zshrc
```

### Turning it off temporarily

To disable drey for one shell without uninstalling anything:

```sh
export PATH="${PATH#$HOME/.drey/bin:}"
```

To disable it for everything, comment out the `export PATH` line inside the drey
block in your shell rc and open a new shell. Backups of every file the installer
edited sit next to the originals as `<file>.backup-<timestamp>`.

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

**A server is missing after install.** `drey install` only wraps what it found
on `PATH` at the time. Install the server, then re-run it.

**Memory did not drop.** Check `drey status`: if you see several backends for
what you thought was one workspace, the roots differ. Git worktrees are separate
by design. Different `initializationOptions` also split, on purpose.
