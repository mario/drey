#!/usr/bin/env bash
#
# Reverts scripts/install.sh. The wrapper and PATH removal lives in
# `drey uninstall`; this script adds the parts that only make sense for a
# from-source install: stopping the daemon and removing the binary itself.
#
#   --keep-binary   leave the `drey` executable installed
#
# Flags other than --keep-binary are passed through to `drey uninstall`
# (e.g. --dry-run).

set -euo pipefail

BIN_DIR="$HOME/.drey/bin"

KEEP_BINARY=0
DRY_RUN=0
PASSTHROUGH=()
for arg in "$@"; do
  case "$arg" in
    --keep-binary) KEEP_BINARY=1 ;;
    --dry-run) DRY_RUN=1; PASSTHROUGH+=("$arg") ;;
    *) PASSTHROUGH+=("$arg") ;;
  esac
done

DREY_BIN="$BIN_DIR/drey"
[ -x "$DREY_BIN" ] || DREY_BIN="$(command -v drey 2>/dev/null || true)"
if [ -z "$DREY_BIN" ] || [ ! -x "$DREY_BIN" ]; then
  echo "no drey binary found; nothing to uninstall" >&2
  exit 0
fi

# Stop the daemon and every server it owns before removing the binary that
# knows how to talk to it.
if [ "$DRY_RUN" -eq 0 ]; then
  "$DREY_BIN" stop >/dev/null 2>&1 && echo "  stopped the daemon and its servers" \
    || echo "  no daemon was running"
fi

"$DREY_BIN" uninstall ${PASSTHROUGH[@]+"${PASSTHROUGH[@]}"}

if [ "$KEEP_BINARY" -eq 0 ] && [ "$DRY_RUN" -eq 0 ]; then
  rm -rf "$HOME/.drey"
  echo "  removed the drey binary and $HOME/.drey"
  # In case an earlier version installed to the default cargo root.
  cargo uninstall drey >/dev/null 2>&1 && echo "  also removed a cargo-installed copy" || true
fi
