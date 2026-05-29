#!/bin/bash
# Generate client bindings for one SpacetimeDB module.
#
# Usage: generate-bindings.sh <module-name>
#
# Reads the module from `/workspace/server/modules/<module>/` and emits:
#   - TypeScript bindings for the pixijs client, to
#     `/workspace/pixijs/src/server/spacetime/bindings/<module>/`
#   - Rust bindings for the gateway, to
#     `/workspace/gateway/src/bindings/<module>/`
#
# Driven by `bin/st build` — see the bindings compose service. Both the
# pixijs and gateway folders are bind-mounted into the container (see
# spacetime/compose.yml).
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
GATEWAY_BINDINGS_DIR="$SCRIPT_DIR/../gateway/src/bindings"
GATEWAY_OUT_DIR="$GATEWAY_BINDINGS_DIR/$MODULE"

if [ ! -d "$MODULE_DIR" ]; then
  echo "Missing module directory: $MODULE_DIR"
  exit 1
fi

# --- TypeScript bindings (pixijs client) ---
mkdir -p "$PIXI_OUT_DIR"
echo "Generating TypeScript bindings for module: $MODULE"
spacetime generate --yes --lang typescript --out-dir "$PIXI_OUT_DIR" --module-path "$MODULE_DIR"

# --- Rust bindings (gateway) ---
mkdir -p "$GATEWAY_OUT_DIR"
echo "Generating Rust bindings for module: $MODULE"
spacetime generate --yes --lang rust --out-dir "$GATEWAY_OUT_DIR" --module-path "$MODULE_DIR"

# Rust (unlike TS) needs each module declared to be reachable. Keep
# `bindings/mod.rs` in sync by ensuring this module's `pub mod` line exists.
MOD_RS="$GATEWAY_BINDINGS_DIR/mod.rs"
touch "$MOD_RS"
if ! grep -q "^pub mod $MODULE;" "$MOD_RS"; then
  echo "pub mod $MODULE;" >> "$MOD_RS"
fi

echo "Done."
echo "TypeScript bindings: $PIXI_OUT_DIR"
echo "Rust bindings:       $GATEWAY_OUT_DIR"
