#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")"/..

usage() {
    (
        { set +x; } 2>/dev/null

        cat <<-EOF
			Usage:
			    # Optional
			    export ANDROID_SERIAL="...";

			    CARGO_PKG="..." \\
			    EXAMPLE="..." \\
			    FEATURES="..." \\
			    "${0}"

			Examples:
			    CARGO_PKG="dial9-perf-self-profile" \\
			    EXAMPLE="basic" \\
			    FEATURES="" \\
			    "${0}"
			or:
			    CARGO_PKG="dial9-tokio-telemetry" \\
			    EXAMPLE="cpu_profile_workload" \\
			    FEATURES="-Fcpu-profiling" \\
			    "${0}"

		EOF

        false
    ) >&2
}

set -x

pwd

ANDROID_SERIAL="${ANDROID_SERIAL:-}" # Optional, to ensure `adb` commands are not ambiguous
CARGO_PKG="${CARGO_PKG:?missing. $(usage)}"
EXAMPLE="${EXAMPLE:?missing. $(usage)}"
FEATURE_ARGS=()
if [[ -n "${FEATURES:-}" ]]; then
    read -r -a FEATURE_ARGS <<< "${FEATURES}"
fi

TARGET="aarch64-linux-android" # Hard-coded
ANDROID_API="${ANDROID_API:-21}"

NDK_HOME="${NDK_HOME:?}"  # used to locate cross-compilation helpers
TOOLCHAIN_BINS=("$NDK_HOME"/toolchains/llvm/prebuilt/*/bin)
TOOLCHAIN_BIN="${TOOLCHAIN_BINS[0]}"

export AR="$TOOLCHAIN_BIN/llvm-ar"
export RANLIB="$TOOLCHAIN_BIN/llvm-ranlib"
export CC_aarch64_linux_android="$TOOLCHAIN_BIN/aarch64-linux-android${ANDROID_API}-clang"
export CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER="${CC_aarch64_linux_android}"

if [[ ! -x "$AR" || ! -x "$RANLIB" || ! -x "$CC_aarch64_linux_android" ]]; then
    echo "Android NDK toolchain not found under $TOOLCHAIN_BIN" >&2
    exit 1
fi

# Cargo build

CARGO_BUILD_CMD=(
    cargo build
        # The package; ~ `Cargo.toml` file/dir
        -p "${CARGO_PKG}"
        # its features to be enabling
        "${FEATURE_ARGS[@]}"
        # The specific `examples/${EXAMPLE}.rs` file to be targeting
        --example "${EXAMPLE}"
        # We are cross-compiling to `aarch64-linux-android`
        --target "${TARGET}"
); "${CARGO_BUILD_CMD[@]}"

# ADB

adb push \
    "target/$TARGET/debug/examples/$EXAMPLE" \
    /data/local/tmp/ \
;

if [[ -n "${DIAL9_FORCE_CTIMER:-}" ]]; then
    adb shell "cd /data/local/tmp && DIAL9_FORCE_CTIMER=1 './$EXAMPLE'" >&2
else
    adb shell "cd /data/local/tmp && './$EXAMPLE'" >&2
fi

adb shell "cd /data/local/tmp && echo Files: && ls \$PWD/* && echo" >&2

{ set +x; } 2>/dev/null

cat <<-'EOF' >&2
	Done.

	You may open a shell at the desired location by doing:

	    adb shell
	    # In the so-obtained shell:
	    cd /data/local/tmp/

	You may also pull the files from there by running:

	    adb pull \
	        /data/local/tmp/cpu_profile_trace.0.bin.gz \
	        ~/Downloads/cpu_profile_trace_"$(date +"%Y-%m-%d_%H-%M-%S")".bin.gz \
	    ;
EOF
