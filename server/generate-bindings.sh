#!/bin/bash
set -e

SCRIPT_DIR="$(realpath "$(dirname "${BASH_SOURCE[0]}")")"

SPACETIMEDB_DIR="$SCRIPT_DIR/spacetimedb"
PIXI_OUT_DIR="$SCRIPT_DIR/../pixijs/src/spacetime/bindings"

if [ ! -d "$SPACETIMEDB_DIR" ]; then
  echo "Missing SpaceTimeDB project directory: $SPACETIMEDB_DIR"
  exit 1
fi

mkdir -p "$PIXI_OUT_DIR"

echo "Generating TypeScript bindings..."
spacetime generate --lang typescript --out-dir "$PIXI_OUT_DIR" --module-path "$SPACETIMEDB_DIR"

echo "Done."
echo "TypeScript bindings: $PIXI_OUT_DIR"
