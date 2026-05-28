#!/usr/bin/env bash
set -euo pipefail

# Generate diagnostic test traces for the dial9 setup diagnostic skill.
# Produces traces with common misconfigurations so the JS skill can be tested.

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
OUTPUT_DIR="${1:-/tmp/dial9-diagnostic-traces}"

echo "Generating diagnostic traces in $OUTPUT_DIR..."
rm -rf "$OUTPUT_DIR"
mkdir -p "$OUTPUT_DIR"

# 1. Good trace + no-wake-events + no-sched-events (normal build with frame pointers)
echo "=== Building with frame pointers + debug symbols ==="
RUSTFLAGS="--cfg tokio_unstable -C force-frame-pointers=yes" \
  cargo build --release --features cpu-profiling \
  --example generate_diagnostic_traces \
  --manifest-path "$REPO_ROOT/dial9-tokio-telemetry/Cargo.toml"

RUSTFLAGS="--cfg tokio_unstable -C force-frame-pointers=yes" \
  cargo run --release --features cpu-profiling \
  --example generate_diagnostic_traces \
  --manifest-path "$REPO_ROOT/dial9-tokio-telemetry/Cargo.toml" \
  -- "$OUTPUT_DIR"

# 2. Missing frame pointers: build WITHOUT -C force-frame-pointers=yes
echo ""
echo "=== Building WITHOUT frame pointers ==="
mkdir -p "$OUTPUT_DIR/no-frame-pointers"
RUSTFLAGS="--cfg tokio_unstable" \
  cargo build --release --features cpu-profiling \
  --example generate_diagnostic_traces \
  --manifest-path "$REPO_ROOT/dial9-tokio-telemetry/Cargo.toml"

# Run just the "good" config but without frame pointers — stacks will be shallow
RUSTFLAGS="--cfg tokio_unstable" \
  cargo run --release --features cpu-profiling \
  --example generate_diagnostic_traces \
  --manifest-path "$REPO_ROOT/dial9-tokio-telemetry/Cargo.toml" \
  -- "$OUTPUT_DIR/no-frame-pointers-tmp"

# Move the "good" trace (which now has bad stacks) to no-frame-pointers
mv "$OUTPUT_DIR/no-frame-pointers-tmp/good"/* "$OUTPUT_DIR/no-frame-pointers/"
rm -rf "$OUTPUT_DIR/no-frame-pointers-tmp"

# 3. Missing debug symbols: build with strip=symbols
echo ""
echo "=== Building WITHOUT debug symbols (stripped) ==="
mkdir -p "$OUTPUT_DIR/no-debug-symbols"

# Use a custom profile-like approach: build with strip
RUSTFLAGS="--cfg tokio_unstable -C force-frame-pointers=yes -C strip=symbols" \
  cargo build --release --features cpu-profiling \
  --example generate_diagnostic_traces \
  --manifest-path "$REPO_ROOT/dial9-tokio-telemetry/Cargo.toml"

RUSTFLAGS="--cfg tokio_unstable -C force-frame-pointers=yes -C strip=symbols" \
  cargo run --release --features cpu-profiling \
  --example generate_diagnostic_traces \
  --manifest-path "$REPO_ROOT/dial9-tokio-telemetry/Cargo.toml" \
  -- "$OUTPUT_DIR/no-debug-symbols-tmp"

mv "$OUTPUT_DIR/no-debug-symbols-tmp/good"/* "$OUTPUT_DIR/no-debug-symbols/"
rm -rf "$OUTPUT_DIR/no-debug-symbols-tmp"

echo ""
echo "=== Done ==="
echo "Traces generated:"
echo "  $OUTPUT_DIR/good/              — fully configured (reference)"
echo "  $OUTPUT_DIR/no-frame-pointers/ — stacks are 1-2 frames deep"
echo "  $OUTPUT_DIR/no-wake-events/    — tasks not instrumented"
echo "  $OUTPUT_DIR/no-debug-symbols/  — symbols are hex addresses only"
echo "  $OUTPUT_DIR/no-sched-events/   — no off-CPU scheduling samples"
