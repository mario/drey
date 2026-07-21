#!/usr/bin/env bash
#
# From-source install: builds the `drey` binary, then hands over to
# `drey install`, which does the real work (finding language servers, writing
# the user config file, the wrappers in ~/.drey/bin, and the PATH block).
#
# If you already have drey from Homebrew or `cargo install`, skip this script
# and run `drey install` directly. Flags are passed straight through.

set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

echo
echo "building drey..."
# Install to an explicit root. Version managers and cargo wrappers redirect the
# default install location, and the wrappers need a path certain to be correct.
cargo install --path "$REPO" --root "$HOME/.drey" --force --quiet

DREY="$HOME/.drey/bin/drey"
if [ ! -x "$DREY" ]; then
  echo "drey built, but $DREY is missing" >&2
  exit 1
fi
echo "installed: $DREY"

exec "$DREY" install "$@"
