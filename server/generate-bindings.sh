#!/bin/bash
# Generate TypeScript bindings for one SpacetimeDB module.
#
# Usage: generate-bindings.sh <module-name>
#
# Reads the module from `/workspace/server/modules/<module>/` and
# writes bindings to `/workspace/pixijs/src/server/spacetime/bindings/<module>/`.
# Driven by `bin/st build` — see the bindings compose service.
set -e

SCRIPT_DIR="$(realpath "$(dirname "${BASH_SOURCE[0]}")")"

MODULE="${1:-}"
if [ -z "$MODULE" ]; then
  echo "usage: generate-bindings.sh <module>"
  echo "       e.g. generate-bindings.sh shard"
  exit 1
fi

MODULE_DIR="$SCRIPT_DIR/modules/$MODULE"
PIXI_OUT_DIR="$SCRIPT_DIR/../pixijs/src/server/spacetime/bindings/$MODULE"

if [ ! -d "$MODULE_DIR" ]; then
  echo "Missing module directory: $MODULE_DIR"
  exit 1
fi

mkdir -p "$PIXI_OUT_DIR"

echo "Generating TypeScript bindings for module: $MODULE"
spacetime generate --yes --lang typescript --out-dir "$PIXI_OUT_DIR" --module-path "$MODULE_DIR"
echo "Done."
echo "TypeScript bindings: $PIXI_OUT_DIR"
