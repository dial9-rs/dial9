#!/usr/bin/env bash
set -e

FLAG_PROFILE=""
while [[ $# -gt 0 ]]; do
    case "$1" in
        --aws-profile=*) FLAG_PROFILE="${1#*=}"; shift ;;
        --aws-profile) FLAG_PROFILE="$2"; shift 2 ;;
        *) echo "Unknown option: $1" >&2; exit 1 ;;
    esac
done

if [ -n "$FLAG_PROFILE" ]; then
    export AWS_PROFILE="$FLAG_PROFILE"
elif [ -z "$AWS_PROFILE" ]; then
    echo "Error: No AWS profile specified." >&2
    echo "Either pass --aws-profile=<profile> or set the AWS_PROFILE environment variable." >&2
    exit 1
fi

REPO_ROOT="$(git -C "$(dirname "$0")" rev-parse --show-toplevel)"
cd "$REPO_ROOT"

TRACE_PATH="$REPO_ROOT/sched-trace.bin"
DEMO_DEST="$REPO_ROOT/dial9-viewer/ui/demo-trace.bin"
# RotatingWriter turns "sched-trace.bin" into "sched-trace.0.bin.gz", etc.
TRACE_GZ_GLOB="$REPO_ROOT/sched-trace.*.bin.gz"

echo "Building metrics-service..."
cargo build --release -p metrics-service

echo "Cleaning old traces..."
rm -f $TRACE_GZ_GLOB "$DEMO_DEST"

echo "Recording demo trace..."
cargo run --release -p metrics-service --bin metrics-service -- \
    --trace-path "$TRACE_PATH" --demo

# Concatenate all segments (sorted by index) into a single trace file.
# When rotation occurs mid-run, early events (like TaskSpawn) end up in
# earlier segments; concatenation preserves the complete timeline.
# We decompress each segment and re-gzip as a single stream to avoid
# multi-member gzip compatibility issues with older Node.js zlib.
SEGMENTS=$(ls -1v $TRACE_GZ_GLOB 2>/dev/null)
if [ -z "$SEGMENTS" ]; then
    echo "ERROR: No trace file generated" >&2
    exit 1
fi

zcat $SEGMENTS | gzip > "$DEMO_DEST"
rm -f $TRACE_GZ_GLOB

echo "Demo trace size:"
ls -lh "$DEMO_DEST"

# Regenerate the JS property fixture from the new trace. This fixture is the
# committed oracle that both `parser_parity_test` (offline fallback) and the
# `aggregate_test` per-filter expectations read, so it MUST be refreshed in the
# same step as the trace — otherwise a regen silently rots both.
PROPS_DEST="$REPO_ROOT/dial9-viewer/tests/fixtures/demo-trace.properties.json"
PROPS_SCRIPT="$REPO_ROOT/dial9-viewer/ui/trace_properties.js"
if command -v node >/dev/null 2>&1; then
    echo "Regenerating property fixture..."
    node "$PROPS_SCRIPT" "$DEMO_DEST" > "$PROPS_DEST"
    echo "✓ Property fixture regenerated: $PROPS_DEST"
    PROPS_HINT="  git add dial9-viewer/tests/fixtures/demo-trace.properties.json"
else
    echo "WARNING: node not found — could NOT regenerate the property fixture." >&2
    echo "         demo-trace.properties.json is now STALE relative to the trace." >&2
    echo "         Run this once node is available:" >&2
    echo "           node $PROPS_SCRIPT $DEMO_DEST > $PROPS_DEST" >&2
    PROPS_HINT="  # then: node dial9-viewer/ui/trace_properties.js dial9-viewer/ui/demo-trace.bin > dial9-viewer/tests/fixtures/demo-trace.properties.json"
fi

echo ""
echo "✓ Demo trace regenerated successfully!"
echo ""
echo "To commit:"
echo "  git add dial9-viewer/ui/demo-trace.bin"
echo "$PROPS_HINT"
echo "  git commit -m 'Regenerate demo trace'"
