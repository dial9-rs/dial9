#!/usr/bin/env bash
# sample-rate-experiment.sh — compare alloc sample rates vs trace overhead
# Usage: AWS_PROFILE=rcoh ./scripts/sample-rate-experiment.sh
set -e

if [ -z "$AWS_PROFILE" ]; then
    echo "Warning: AWS_PROFILE not set. Using default AWS credentials." >&2
fi

REPO_ROOT="$(git -C "$(dirname "$0")" rev-parse --show-toplevel)"
cd "$REPO_ROOT"

WORK_DIR="$(mktemp -d /tmp/dial9-sample-rate-exp.XXXXXX)"
trap 'rm -rf "$WORK_DIR"' EXIT

BINARY="$REPO_ROOT/target/release/metrics-service"

echo "Building metrics-service (release)..."
cargo build --release -p metrics-service 2>&1 | grep -E "Compiling|Finished"

declare -a LABELS=("baseline (no profiling)" "8 MiB" "2 MiB" "512 KiB" "64 KiB")
declare -a RATES=(""  "8388608" "2097152" "524288" "65536")

printf "\nRunning %d experiments (--demo --leak, ~4s each)...\n\n" "${#LABELS[@]}"

printf "%-26s %8s %10s %10s %12s %10s %10s\n" \
    "Configuration" "TraceKB" "AllocEvts" "FreeEvts" "SampledMB" "GET p50" "GET p99"
printf "%-26s %8s %10s %10s %12s %10s %10s\n" \
    "─────────────────────────" "───────" "─────────" "─────────" "──────────" "───────" "───────"

for i in "${!LABELS[@]}"; do
    label="${LABELS[$i]}"
    rate="${RATES[$i]}"
    trace_dir="$WORK_DIR/run_$i"
    client_out="$WORK_DIR/client_$i.json"
    mkdir -p "$trace_dir"

    args=(--trace-path "$trace_dir" --demo --leak --no-task-dumps)
    if [ -z "$rate" ]; then
        args+=(--no-memory-profiling)
    else
        args+=(--alloc-sample-rate-bytes "$rate")
    fi

    # Capture stdout (client JSON) separately from stderr (service logs)
    "$BINARY" "${args[@]}" >"$client_out" 2>/dev/null

    # Trace size
    trace_bytes=$(find "$trace_dir" -name "*.bin.gz" -o -name "*.bin" | xargs wc -c 2>/dev/null | tail -1 | awk '{print $1}')
    trace_kb=$(( ${trace_bytes:-0} / 1024 ))

    # Event counts
    read -r alloc_count free_count total_alloc_bytes < <(node -e "
const {parseTrace} = require('$REPO_ROOT/dial9-viewer/ui/trace_parser.js');
const fs = require('fs');
const files = fs.readdirSync('$trace_dir').filter(f => f.match(/\.(gz|bin)$/)).sort();
let alloc=0, free=0, bytes=0;
(async () => {
  for (const f of files) {
    const t = await parseTrace(fs.readFileSync('$trace_dir/' + f));
    alloc += t.allocEvents.length;
    free += t.freeEvents.length;
    bytes += t.allocEvents.reduce((s,a)=>s+a.size,0);
  }
  process.stdout.write(alloc + ' ' + free + ' ' + bytes + '\n');
})().catch(() => process.stdout.write('0 0 0\n'));
" 2>/dev/null)

    sampled_mb=$(( ${total_alloc_bytes:-0} / 1024 / 1024 ))

    # Latency from client JSON (last JSON object in output)
    read -r get_p50 get_p99 < <(node -e "
const fs = require('fs');
const txt = fs.readFileSync('$client_out', 'utf8');
// Find the last top-level JSON object (the final summary)
let depth=0, start=-1, last=null;
for (let i=0;i<txt.length;i++) {
  if (txt[i]==='{') { if(depth===0) start=i; depth++; }
  else if (txt[i]==='}') { depth--; if(depth===0 && start>=0) { try{last=JSON.parse(txt.slice(start,i+1));}catch{} } }
}
const g = last && last.GET;
process.stdout.write((g ? g.p50_ms.toFixed(1) : '?') + ' ' + (g ? g.p99_ms.toFixed(1) : '?') + '\n');
" 2>/dev/null)

    printf "%-26s %8d %10d %10d %10d MB %7sms %7sms\n" \
        "$label" "$trace_kb" "${alloc_count:-0}" "${free_count:-0}" "$sampled_mb" "$get_p50" "$get_p99"
done

echo ""
