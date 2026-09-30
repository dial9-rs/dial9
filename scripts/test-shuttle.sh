#!/usr/bin/env bash
set -euo pipefail

# Guards against a crate's shuttle invocation silently matching 0 tests
# (e.g. after a test module gets renamed/removed) and reporting success
# anyway.
run_shuttle_tests() {
  local pkg="$1"
  shift
  local output
  output=$("$@" 2>&1 | tee /dev/stderr)
  if grep -qE '^running 0 tests$' <<<"$output"; then
    echo "error: shuttle tests for '$pkg' matched 0 tests — shuttle coverage for this crate has silently regressed to nothing" >&2
    exit 1
  fi
}

run_shuttle_tests dial9-core \
  env RUSTFLAGS="--cfg shuttle" \
  cargo test -p dial9-core --lib --features _shuttle -- shuttle "$@"

run_shuttle_tests dial9-tokio-telemetry \
  env RUSTFLAGS="--cfg tokio_unstable --cfg shuttle" \
  cargo test -p dial9-tokio-telemetry --lib --features _shuttle -- shuttle "$@"

run_shuttle_tests dial9-utils \
  env RUSTFLAGS="--cfg tokio_unstable --cfg shuttle" \
  cargo test -p dial9-utils --lib --features _shuttle -- shuttle "$@"
