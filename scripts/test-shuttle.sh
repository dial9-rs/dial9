#!/usr/bin/env bash
set -euo pipefail

RUSTFLAGS="--cfg shuttle" \
  cargo test -p dial9-core --lib --features _shuttle -- shuttle "$@"

telemetry_features=_shuttle
if [[ "$(uname -s)" == Linux ]]; then
  telemetry_features+=,taskdump
fi

RUSTFLAGS="--cfg tokio_unstable --cfg shuttle" \
  cargo test -p dial9-tokio-telemetry --lib --features "$telemetry_features" -- shuttle "$@"

RUSTFLAGS="--cfg tokio_unstable --cfg shuttle" \
  cargo test -p dial9-utils --lib --features _shuttle -- shuttle "$@"
