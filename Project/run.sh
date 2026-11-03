#!/usr/bin/env bash
# Launch the Rust WorkshopDL-Linux app from its project folder.
set -euo pipefail

PROJECT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$PROJECT_DIR/.." && pwd)"
BIN="$ROOT/target/release/workshopdl-linux"

if [ ! -x "$BIN" ] || [ "${WORKSHOPDL_REBUILD:-0}" = "1" ]; then
    if [ -f "$HOME/.cargo/env" ]; then
        # shellcheck disable=SC1091
        source "$HOME/.cargo/env"
    fi
    (cd "$ROOT" && cargo build --release)
fi

export WORKSHOPDL_PROJECT_DIR="$PROJECT_DIR"
exec "$BIN" "$@"
