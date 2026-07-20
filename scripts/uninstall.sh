#!/usr/bin/env bash
#
# Reverts everything scripts/install.sh did. Safe to run more than once, and
# safe to run even if install.sh only partly completed.
#
#   --keep-binary   leave the `drey` executable installed
#   --keep-config   leave ~/.config/drey/config.toml in place

set -euo pipefail

BIN_DIR="$HOME/.drey/bin"
CONFIG_DIR="$HOME/.config/drey"
MARK_BEGIN="# >>> drey >>>"
MARK_END="# <<< drey <<<"

KEEP_BINARY=0
KEEP_CONFIG=0
for arg in "$@"; do
  case "$arg" in
    --keep-binary) KEEP_BINARY=1 ;;
    --keep-config) KEEP_CONFIG=1 ;;
    *) echo "unknown option: $arg" >&2; exit 2 ;;
  esac
done

say() { printf '  %s\n' "$*"; }

echo
echo "drey uninstall"
echo

# 1. Stop the daemon and every server it owns, before removing the binary that
#    knows how to talk to it.
DREY_BIN="$BIN_DIR/drey"
[ -x "$DREY_BIN" ] || DREY_BIN="$(command -v drey 2>/dev/null || true)"
if [ -n "$DREY_BIN" ] && [ -x "$DREY_BIN" ]; then
  "$DREY_BIN" stop >/dev/null 2>&1 && say "stopped the daemon and its servers" \
    || say "no daemon was running"
fi

# 2. Remove the wrappers. This alone is enough to restore normal behaviour:
#    with them gone, PATH lookups fall through to the real servers again.
if [ -d "$BIN_DIR" ]; then
  count=$(find "$BIN_DIR" -maxdepth 1 -type f ! -name drey | wc -l | tr -d ' ')
  say "removing $count wrapper(s) from $BIN_DIR"
else
  say "no wrappers to remove"
fi

# 3. Take the PATH block back out of the shell rc files.
for RC in "$HOME/.zshenv" "$HOME/.zshrc" "$HOME/.bashrc" "$HOME/.profile"; do
  [ -f "$RC" ] || continue
  grep -qF "$MARK_BEGIN" "$RC" || continue
  cp "$RC" "$RC.backup-$(date +%Y%m%d-%H%M%S)"
  # Delete the marked block, and the blank line that precedes it.
  awk -v b="$MARK_BEGIN" -v e="$MARK_END" '
    $0 == b { skip = 1; if (blank) blank = 0; next }
    $0 == e { skip = 0; next }
    skip { next }
    { print }
  ' "$RC" > "$RC.drey-tmp" && mv "$RC.drey-tmp" "$RC"
  say "removed PATH entry from $RC (backup alongside it)"
done

# 4. Config, unless asked to keep it.
if [ "$KEEP_CONFIG" -eq 0 ] && [ -d "$CONFIG_DIR" ]; then
  rm -rf "$CONFIG_DIR"
  say "removed $CONFIG_DIR"
elif [ "$KEEP_CONFIG" -eq 1 ]; then
  say "kept $CONFIG_DIR"
fi

# 5. Runtime state: socket, logs.
for d in "$HOME/Library/Caches/drey" "$HOME/.local/state/drey" \
         "${XDG_RUNTIME_DIR:-/nonexistent}/drey"; do
  [ -d "$d" ] && rm -rf "$d" && say "removed runtime state $d"
done

# 6. The binary itself, and the directory holding it.
if [ "$KEEP_BINARY" -eq 0 ]; then
  rm -rf "$HOME/.drey"
  say "removed the drey binary and $HOME/.drey"
  # In case an earlier version installed to the default cargo root.
  cargo uninstall drey >/dev/null 2>&1 && say "also removed a cargo-installed copy" || true
else
  # Keep drey itself but drop the wrappers, so nothing is intercepted.
  find "$BIN_DIR" -maxdepth 1 -type f ! -name drey -delete 2>/dev/null || true
  say "kept the drey binary at $BIN_DIR/drey; wrappers removed"
fi

echo
echo "Done. Open a new shell so PATH is rebuilt, then confirm:"
echo "    which rust-analyzer     # should be the real one again"
echo
echo "Any language servers you had running under drey were stopped; your"
echo "editors and agents will start their own again on next use."
echo
