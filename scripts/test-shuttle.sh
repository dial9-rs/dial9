#!/usr/bin/env bash
set -euo pipefail

RUSTFLAGS="--cfg shuttle" \
  cargo test -p dial9-core --lib --features _shuttle -- shuttle "$@"

RUSTFLAGS="--cfg tokio_unstable --cfg shuttle" \
  cargo test -p dial9-tokio-telemetry --lib --features _shuttle -- shuttle "$@"
