#!/usr/bin/env bash
#
# e2e-trace-tests.sh -- End-to-end trace pipeline test
#
# Regenerates the demo trace against a local DynamoDB, then runs the
# JS-based trace integrity and analysis test suites.
#
#
# Usage:
#   scripts/e2e-trace-tests.sh                  # default (DDB on :8000)
#   scripts/e2e-trace-tests.sh --ddb-port=4566  # custom DDB port
#
# Options:
#   --ddb-port PORT  Port where DynamoDB Local is listening (default: 8000).
set -euo pipefail

DDB_PORT=8000
while [[ $# -gt 0 ]]; do
    case "$1" in
        --ddb-port=*) DDB_PORT="${1#*=}"; shift ;;
        --ddb-port)   DDB_PORT="$2"; shift 2 ;;
        *) echo "Unknown option: $1" >&2; exit 1 ;;
    esac
done

REPO_ROOT="$(git -C "$(dirname "$0")" rev-parse --show-toplevel)"
cd "$REPO_ROOT"

export AWS_ENDPOINT_URL="${AWS_ENDPOINT_URL:-http://localhost:$DDB_PORT}"
export AWS_ACCESS_KEY_ID="${AWS_ACCESS_KEY_ID:-local}"
export AWS_SECRET_ACCESS_KEY="${AWS_SECRET_ACCESS_KEY:-local}"
export AWS_DEFAULT_REGION="${AWS_DEFAULT_REGION:-us-east-1}"
export AWS_PROFILE="${AWS_PROFILE:-fake-profile}"

echo "--- Regenerating demo trace ---"
scripts/regenerate_demo_trace.sh

echo "--- Checking trace integrity ---"
node dial9-viewer/ui/test_trace_integrity.js

echo "--- Checking task lifecycle consistency logic ---"
node dial9-viewer/ui/test_task_lifecycle.js

echo "--- Checking trace analysis ---"
node dial9-viewer/ui/test_trace_analysis.js

echo "--- Checking trace property oracle (Rust decode parity reference) ---"
node dial9-viewer/ui/test_trace_properties.js

echo "--- Checking multi-component trace fetch (repeatable trace=) ---"
node dial9-viewer/ui/test_fetch_traces.js

echo "--- Checking streaming trace decode (parseTraceStream) ---"
node dial9-viewer/ui/test_stream_parse.js

echo "--- Checking buffered parser paint-yield throttle (#595) ---"
node dial9-viewer/ui/test_parse_yield_throttle.js

echo "--- Checking bring-your-own-credentials store ---"
node dial9-viewer/ui/test_creds.js

echo "--- Checking landing-page URL state (serialize/parse) ---"
node dial9-viewer/ui/test_url_state.js

echo "--- Checking trace scope (encode/resolve, URI-cap safety) ---"
node dial9-viewer/ui/test_trace_scope.js

echo "--- Checking skills snippets ---"
node dial9-viewer/ui/test_all_skills_snippets.js

echo "--- Checking flamegraph API refinement helpers ---"
node dial9-viewer/ui/test_flamegraph_api.js

echo "--- Checking tokio-stats API helpers (exemplar link + coverage) ---"
node dial9-viewer/ui/test_tokio_stats_api.js

echo "--- Checking SSE frame decoder ---"
node dial9-viewer/ui/test_sse.js

echo "--- Checking prefix detection ---"
node dial9-viewer/ui/test_prefix_detection.js

echo "--- Checking enclosing spans (per-worker) ---"
node dial9-viewer/ui/test_enclosing_spans.js

echo "--- Checking flamegraph export (folded + SVG) ---"
node dial9-viewer/ui/test_flamegraph_export.js

echo "--- Checking runtime grouping (multi-runtime lanes) ---"
node dial9-viewer/ui/test_runtime_groups.js

echo "All E2E trace checks passed."
