#!/usr/bin/env bash
#
# dev.sh — run the Mahi CLI in development mode.
#
# Defaults to the zero-config developer setup: in-memory store + mock
# inference provider (no API keys, no disk state, no network). Pass your own
# arguments to override the defaults entirely:
#
#   ./scripts/dev.sh                          # in-memory store, on-device mock
#   ./scripts/dev.sh --store ./mahi-data      # encrypted on-disk store
#   ANTHROPIC_API_KEY=… cargo run -p mahi-cli --features hosted -- --model claude-fable-5
#
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

DEFAULT_ARGS=(--store memory)

if [[ $# -gt 0 ]]; then
    ARGS=("$@")
else
    ARGS=("${DEFAULT_ARGS[@]}")
fi

# Friendly default log level for development; respect an existing RUST_LOG.
export RUST_LOG="${RUST_LOG:-info}"

echo "==> cargo run -p mahi-cli -- ${ARGS[*]}"
exec cargo run -p mahi-cli -- "${ARGS[@]}"
