#!/usr/bin/env bash
set -euo pipefail

# Accumulates coverage across all three shuttle-tested crates into one
# report, mirroring test-shuttle.sh's per-crate RUSTFLAGS.
cargo llvm-cov clean --workspace

RUSTFLAGS="--cfg shuttle" \
  cargo llvm-cov --no-report --lib --features _shuttle -p dial9-core -- shuttle "$@"

RUSTFLAGS="--cfg tokio_unstable --cfg shuttle" \
  cargo llvm-cov --no-report --lib --features _shuttle -p dial9-tokio-telemetry -- shuttle "$@"

RUSTFLAGS="--cfg tokio_unstable --cfg shuttle" \
  cargo llvm-cov --no-report --lib --features _shuttle -p dial9-utils -- shuttle "$@"

cargo llvm-cov report --html

echo "Report: target/llvm-cov/html/index.html"
open target/llvm-cov/html/index.html 2>/dev/null || true
