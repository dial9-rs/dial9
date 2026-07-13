#!/usr/bin/env bash
#
# e2e-trace-tests.sh -- End-to-end trace pipeline test
#
# Regenerates the demo trace against a local DynamoDB, then runs the
# trace-dependent Vitest suites against the freshly regenerated
# dial9-viewer/ui/public/demo-trace.bin.
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

# The trace-dependent suites this script historically ran via `node
# test_*.js`, migrated to Vitest by T10/T11. The full tests/core/ set already
# runs in the `ui` CI job against the COMMITTED demo trace; this filtered
# list is what must ALSO hold against a freshly REGENERATED trace in the DDB
# environment. (Deliberately excluded: flamegraph_recipes.test.ts carries an
# it.fails pinned to the committed trace's data and would invert against a
# regenerated one; directory_parse/directory_scale/slice/etc. are
# demo-trace-shape-independent and covered by the `ui` job.)
TRACE_SUITES=(
    tests/core/trace_integrity.test.ts
    tests/core/task_lifecycle.test.ts
    tests/core/trace_analysis.test.ts
    tests/core/trace_properties.test.ts
    tests/core/fetch_traces.test.ts
    tests/core/stream_parse.test.ts
    tests/core/parse_yield_throttle.test.ts
    tests/core/url_state.test.ts
    tests/core/all_skills_snippets.test.ts
    tests/core/flamegraph_api.test.ts
    tests/core/enclosing_spans.test.ts
    tests/core/flamegraph_export.test.ts
    tests/core/runtime_groups.test.ts
)

echo "--- Running trace-dependent Vitest suites ---"
cd dial9-viewer/ui
if [ ! -d node_modules ]; then
    npm ci
fi
npx vitest run "${TRACE_SUITES[@]}"

echo "All E2E trace checks passed."
