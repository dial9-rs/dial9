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

TRACE_DIR="$REPO_ROOT/sched-traces"
DEMO_DEST="$REPO_ROOT/dial9-viewer/ui/demo-trace.bin"
# The rotating writer names segments
# sched-traces/trace.0.bin.gz, sched-traces/trace.1.bin.gz, etc.
TRACE_GZ_GLOB="$TRACE_DIR/trace.*.bin.gz"

echo "Building metrics-service..."
cargo build --release -p metrics-service

echo "Cleaning old traces..."
rm -rf "$TRACE_DIR" "$DEMO_DEST"

echo "Recording demo trace..."
cargo run --release -p metrics-service --bin metrics-service -- \
    --trace-path "$TRACE_DIR" --demo

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
rm -rf "$TRACE_DIR"

echo "Demo trace size:"
ls -lh "$DEMO_DEST"

# Regenerate the JS property fixture from the new trace. This fixture is the
# committed oracle that both `parser_parity_test` (offline fallback) and the
# `aggregate_test` per-filter expectations read, so it should be refreshed in
# the same step as the trace — otherwise a real regen silently rots both.
#
# BUT only overwrite it when the regenerated trace is a *canonical* capture
# (i.e. it actually contains SchedEvent samples). A profiling-incapable
# environment — notably CI containers, where this script runs in the
# e2e-trace-tests job — produces a CpuProfile-only trace with no sched events
# and run-to-run-variable timing. Committing that degraded trace's properties
# would (a) replace the real digests and (b) defeat `test_trace_properties.js`'s
# guard, which skips its rich cross-check precisely when the on-disk trace's
# sample count differs from the committed fixture. So: regenerate to a temp
# file, and promote it to the committed fixture only if it has sched events.
PROPS_DEST="$REPO_ROOT/dial9-viewer/tests/fixtures/demo-trace.properties.json"
PROPS_SCRIPT="$REPO_ROOT/dial9-viewer/ui/trace_properties.js"
if command -v node >/dev/null 2>&1; then
    PROPS_TMP="$(mktemp)"
    node "$PROPS_SCRIPT" "$DEMO_DEST" > "$PROPS_TMP"
    # by_source["1"] is the SchedEvent count; >0 means a real perf capture.
    # Read the temp file via fs.readFileSync (NOT require) — the mktemp path has
    # no .json suffix, so require() would parse it as CommonJS and choke on JSON.
    SCHED_COUNT="$(node -e 'const fs=require("fs"); const p=JSON.parse(fs.readFileSync(process.argv[1],"utf8")); process.stdout.write(String((p.by_source&&p.by_source["1"])||0))' "$PROPS_TMP")"
    if [ "$SCHED_COUNT" -gt 0 ]; then
        mv "$PROPS_TMP" "$PROPS_DEST"
        echo "✓ Property fixture regenerated ($SCHED_COUNT sched samples): $PROPS_DEST"
        PROPS_HINT="  git add dial9-viewer/tests/fixtures/demo-trace.properties.json"
    else
        rm -f "$PROPS_TMP"
        echo "NOTE: regenerated trace has no SchedEvent samples (profiling-incapable" >&2
        echo "      environment, e.g. CI). Leaving the committed property fixture" >&2
        echo "      untouched — it must reflect a canonical perf capture. Regenerate" >&2
        echo "      the fixture on a perf-capable host to refresh it." >&2
        PROPS_HINT="  # fixture left unchanged (no sched samples in this environment)"
    fi
else
    echo "WARNING: node not found — could NOT regenerate the property fixture." >&2
    echo "         demo-trace.properties.json may be STALE relative to the trace." >&2
    echo "         Run this once node is available, on a perf-capable host:" >&2
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
