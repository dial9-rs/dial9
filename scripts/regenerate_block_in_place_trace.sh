#!/usr/bin/env bash
# Regenerate the block_in_place test trace used by test_block_in_place.js.
# Run from anywhere; the script finds the repo root automatically.
set -e

REPO_ROOT="$(git -C "$(dirname "$0")" rev-parse --show-toplevel)"
cd "$REPO_ROOT"

DEST="dial9-viewer/ui/test-traces/block_in_place.bin"
TRACE_PATH="$REPO_ROOT/block_in_place_trace.bin"
# RotatingWriter appends segment index: block_in_place_trace.0.bin
TRACE_GLOB="$REPO_ROOT/block_in_place_trace.*.bin"

echo "Building block_in_place_workload example..."
cargo build --release --example block_in_place_workload -p dial9-tokio-telemetry --features cpu-profiling

echo "Cleaning old traces..."
rm -f $TRACE_GLOB

echo "Recording trace..."
cargo run --release --example block_in_place_workload -p dial9-tokio-telemetry --features cpu-profiling

# Take the first (and likely only) segment.
SEGMENT=$(ls -1v $TRACE_GLOB 2>/dev/null | head -1)
if [ -z "$SEGMENT" ]; then
    echo "ERROR: No trace file generated" >&2
    exit 1
fi

mkdir -p "$(dirname "$DEST")"
cp "$SEGMENT" "$DEST"
rm -f $TRACE_GLOB

echo "Test trace size:"
ls -lh "$DEST"

echo ""
echo "✓ block_in_place test trace regenerated!"
echo ""
echo "To commit:"
echo "  git add -f $DEST"
echo "  git commit -m 'Regenerate block_in_place test trace'"
