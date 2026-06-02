---
name: dial9-html-report
description: Compile dial9 trace analysis insights into a polished HTML report folder with embedded flamegraphs, timeline strips, and viewer deep-links. Use when you have findings from trace analysis and need to deliver them as something a human can open in a browser.
---

# Building HTML Reports from Trace Insights

## When to use this skill

You have already analyzed a dial9 trace (using the `dial9-toolkit` and `dial9-trace-analysis` skills) and have a set of findings. Now you need to deliver those findings as an HTML report — a folder a human can open in their browser via a tiny local server.

## Viewing the report (important)

Reports are **served, not opened directly**. Browsers block `fetch()` over `file://`, so the embedded flamegraph and timeline iframes will fail if the user opens `report.html` directly from disk. Tell the user (and yourself, when verifying):

```bash
# from the dial9 CLI (recommended)
dial9 report serve path/to/report-folder
# → http://localhost:8000/report.html

# or any static-file server
python3 -m http.server -d path/to/report-folder 8000
```

The folder is portable — zip it, attach it to a PR, drop it in Slack. The recipient just needs to serve it locally too.

## The shape of a report

A report is a **folder**, not a single file:

```
my-report/
├── report.html          # The main report document
├── embed.html           # Embeddable viewer component (copy from dial9-viewer/ui/)
├── flamegraph.js        # Required by embed.html (copy from dial9-viewer/ui/)
├── flamegraph.css       # Required by embed.html (copy from dial9-viewer/ui/)
├── decode.js            # Required by embed.html (copy from dial9-viewer/ui/)
├── trace_parser.js      # Required by embed.html (copy from dial9-viewer/ui/)
├── trace_analysis.js    # Required by embed.html (copy from dial9-viewer/ui/)
├── flamegraphs/         # Standalone filtered flamegraph HTML files (optional)
│   └── task-3-cpu.html  # Example: per-task flamegraph
└── traces/
    ├── burst-window.bin # Sliced trace for the burst finding
    └── startup-io.bin   # Sliced trace for the startup finding
```

Copy `embed.html` and its JS/CSS dependencies from the dial9-viewer `ui/` directory into the report folder. Slice traces into `traces/` so the report is portable and small.

## Writing the report HTML

Write HTML directly — no JSON, no markdown intermediate. Use this style block:

```html
<style>
:root { --bg: #0d1117; --surface: #161b22; --border: #30363d; --text: #e6edf3; --muted: #8b949e; --accent: #58a6ff; --critical: #f85149; --warning: #d29922; --info: #58a6ff; }
* { margin: 0; padding: 0; box-sizing: border-box; }
body { font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', Helvetica, Arial, sans-serif; background: var(--bg); color: var(--text); line-height: 1.6; padding: 2rem; max-width: 1100px; margin: 0 auto; }
h1 { font-size: 1.8rem; margin-bottom: 0.5rem; }
h2 { font-size: 1.4rem; margin: 2rem 0 1rem; border-bottom: 1px solid var(--border); padding-bottom: 0.5rem; }
.meta { color: var(--muted); font-size: 0.9rem; margin-bottom: 2rem; }
.finding { background: var(--surface); border: 1px solid var(--border); border-radius: 8px; padding: 1.5rem; margin-bottom: 1.5rem; }
.finding-header { display: flex; align-items: center; gap: 0.75rem; margin-bottom: 1rem; }
.severity { font-size: 0.75rem; font-weight: 600; text-transform: uppercase; padding: 0.2rem 0.6rem; border-radius: 4px; }
.severity-critical { background: var(--critical); color: #fff; }
.severity-warning { background: var(--warning); color: #000; }
.severity-info { background: var(--info); color: #000; }
.code-ref { font-family: 'SF Mono', monospace; font-size: 0.85rem; background: #1c2128; padding: 0.15rem 0.4rem; border-radius: 3px; }
a { color: var(--accent); text-decoration: none; }
a:hover { text-decoration: underline; }
</style>
```

### Insight card structure

```html
<div class="finding">
  <div class="finding-header">
    <span class="severity severity-critical">Critical</span>
    <h3>Headline describing the problem</h3>
  </div>
  <p>Prose explanation of what was observed, with <span class="code-ref">source.rs:42</span> references.</p>
  <p><strong>Fix:</strong> Actionable recommendation.</p>
  <!-- Optional: embedded visualization -->
  <iframe src="embed.html?trace=traces/sliced.bin&amp;view=flamegraph" width="100%" height="320" style="border:0"></iframe>
</div>
```

**HTML escaping:** Always escape `<`, `>`, `&`, and `"` in any agent-generated text inserted into HTML attributes or element content.

## Embedding visualizations with `embed.html`

`embed.html` renders a single visualization from a trace file. It accepts these URL params:

| Param | Required | Description |
|-------|----------|-------------|
| `trace` | yes | Relative path or URL to a `.bin` or `.bin.gz` trace file |
| `view` | yes | `flamegraph` or `timeline` |
| `start` | no | Filter events to >= this monotonic timestamp (nanoseconds) |
| `end` | no | Filter events to < this monotonic timestamp (nanoseconds) |
| `height` | no | Override container height in pixels (default: 320 for flamegraph, 180 for timeline) |

### Flamegraph embed

Shows an on-CPU flamegraph for the filtered time range:

```html
<iframe src="embed.html?trace=traces/burst.bin&amp;start=3900000000&amp;end=4050000000&amp;view=flamegraph"
        width="100%" height="320" style="border:0"></iframe>
```

### Timeline embed

Shows worker-activity lanes (polls as colored bars, parks as red strips):

```html
<iframe src="embed.html?trace=traces/burst.bin&amp;start=3900000000&amp;end=4050000000&amp;view=timeline"
        width="100%" height="180" style="border:0"></iframe>
```

## Flamegraphs: precise filtering via standalone HTML

### When to use this vs. `embed.html?view=flamegraph`

Use `embed.html?view=flamegraph&start=...&end=...` when the time range alone is the right filter — e.g., "show the burst window." It's one iframe tag and zero JS.

Use a **standalone HTML file** when you need anything more precise:
- A specific task's CPU profile
- Only polls exceeding a duration threshold
- Off-CPU (scheduling) samples only
- Leaf-frame search (e.g., "where does `load_native_certs` appear?")
- Multi-trace union flamegraph
- Any combination of the above

### Skeleton (copy and fill in your filter)

Place standalone flamegraph files in `report/flamegraphs/<name>.html`. They load the same JS modules as `embed.html` but give you full control over filtering.

```html
<!DOCTYPE html>
<html><head><link rel="stylesheet" href="../flamegraph.css"></head><body>
  <div id="fg" style="height:100vh"></div>
  <script src="../decode.js"></script>
  <script src="../trace_parser.js"></script>
  <script src="../trace_analysis.js"></script>
  <script src="../flamegraph.js"></script>
  <script>(async () => {
    const buf = await fetch('../traces/full.bin').then(r => r.arrayBuffer());
    const trace = await TraceParser.parseTrace(buf);
    const workerIds = [...new Set(trace.events.filter(e => e.workerId != null).map(e => e.workerId))];
    const ws = TraceAnalysis.buildWorkerSpans(trace.events, workerIds, trace.maxTs, trace.blockInPlaceGaps);
    TraceAnalysis.attachCpuSamples(trace.cpuSamples, ws.workerSpans);

    // AGENT: replace this filter with your predicate.
    const samples = trace.cpuSamples.filter(s => s.source === 0);

    const el = document.getElementById('fg');
    if (samples.length === 0) {
      const total = trace.cpuSamples.length;
      const onCpu = trace.cpuSamples.filter(s => s.source === 0).length;
      const offCpu = trace.cpuSamples.filter(s => s.source === 1).length;
      const locs = new Map();
      trace.cpuSamples.forEach(s => { if (s.spawnLoc) locs.set(s.spawnLoc, (locs.get(s.spawnLoc)||0)+1); });
      const top3 = [...locs.entries()].sort((a,b) => b[1]-a[1]).slice(0,3).map(([l,c]) => l.split('/').pop()+' ('+c+')').join(', ');
      el.innerHTML = '<pre>No samples matched filter.\nTotal samples: '+total+' (on-CPU: '+onCpu+', off-CPU: '+offCpu+')\nTop spawn locs: '+top3+'</pre>';
      return;
    }
    FlamegraphRenderer.createFlamegraph(el, () => {}).setData(samples, trace.callframeSymbols);
  })();</script>
</body></html>
```

**Key points:**
- `buildWorkerSpans` + `attachCpuSamples` decorates each sample with `.spawnLoc` (the source location where the task was spawned). This is required for any recipe that filters by spawn location.
- Polls in `ws.workerSpans[workerId].polls` have `.taskId`, `.start`, `.end`. Use these to filter samples by task or by poll duration.
- `trace.callframeSymbols` is a `Map<string, entry|entry[]>` where each entry is `{symbol, location}`. Array entries are inlined frames (index 0 = outermost).

### Recipes

#### Recipe: Total on-CPU flamegraph

**Question it answers:** "What is the application spending CPU on?"

```js
const samples = trace.cpuSamples.filter(s => s.source === 0);
```

**When NOT to use:** When you want off-CPU / scheduling delay analysis. Use `source === 1` instead.

---

#### Recipe: Off-CPU (scheduling) samples only

**Question it answers:** "What code was running when the kernel moved my worker thread off-CPU?" Useful for diagnosing blocking calls in async code.

```js
const samples = trace.cpuSamples.filter(s => s.source === 1);
```

**When NOT to use:** When diagnosing high CPU usage — those are on-CPU samples (`source === 0`).

---

#### Recipe: Just task X

**Question it answers:** "What does this specific task's CPU profile look like?"

```js
// Collect poll time ranges for the target task
const TARGET_TASK_ID = 3;
const taskPolls = [];
for (const wid of Object.keys(ws.workerSpans)) {
  for (const p of ws.workerSpans[wid].polls) {
    if (p.taskId === TARGET_TASK_ID) taskPolls.push(p);
  }
}
const samples = trace.cpuSamples.filter(s =>
  s.source === 0 && taskPolls.some(p => s.timestamp >= p.start && s.timestamp <= p.end)
);
```

**When NOT to use:** When the task has very few polls — you'll get too few samples for a useful flamegraph. Check `taskPolls.length` first.

---

#### Recipe: Just polls > N ms

**Question it answers:** "What code paths cause long polls?"

```js
const THRESHOLD_NS = 5_000_000; // 5ms
const longPolls = [];
for (const wid of Object.keys(ws.workerSpans)) {
  for (const p of ws.workerSpans[wid].polls) {
    if ((p.end - p.start) > THRESHOLD_NS) longPolls.push(p);
  }
}
const samples = trace.cpuSamples.filter(s =>
  s.source === 0 && longPolls.some(p => s.timestamp >= p.start && s.timestamp <= p.end)
);
```

**When NOT to use:** When the long polls are idle waits (off-CPU). Switch to `source === 1` if the on-CPU flamegraph looks thin.

---

#### Recipe: One specific poll instance

**Question it answers:** "What happened during the single worst poll of task X?"

```js
const TARGET_TASK_ID = 3;
let worstPoll = null;
for (const wid of Object.keys(ws.workerSpans)) {
  for (const p of ws.workerSpans[wid].polls) {
    if (p.taskId === TARGET_TASK_ID) {
      if (!worstPoll || (p.end - p.start) > (worstPoll.end - worstPoll.start)) worstPoll = p;
    }
  }
}
const samples = trace.cpuSamples.filter(s =>
  s.source === 0 && s.timestamp >= worstPoll.start && s.timestamp <= worstPoll.end
);
```

**When NOT to use:** When the worst poll has < 3 samples — the flamegraph won't be statistically meaningful. Show the raw sample stacks instead.

---

#### Recipe: Leaf-frame search

**Question it answers:** "Which samples have a specific function at the leaf (i.e., the function was actually on-CPU)?"

```js
const SEARCH = 'load_native_certs';
const samples = trace.cpuSamples.filter(s => {
  const leaf = s.callchain[s.callchain.length - 1];
  const sym = trace.callframeSymbols.get(leaf);
  if (!sym) return false;
  // Handle both plain entries and inlined-frame arrays
  const name = Array.isArray(sym) ? sym[0].symbol : sym.symbol;
  return name && name.includes(SEARCH);
});
```

**When NOT to use:** When you want to find a function *anywhere* in the stack (not just at the leaf). Remove the `[s.callchain.length - 1]` indexing and iterate all frames with `.some()` instead.

---

#### Recipe: Filter by spawn location

**Question it answers:** "What does the CPU profile look like for all tasks spawned from a specific call site?"

```js
const SPAWN_LOC = 'src/main.rs:260:14'; // substring match
const samples = trace.cpuSamples.filter(s =>
  s.source === 0 && s.spawnLoc && s.spawnLoc.includes(SPAWN_LOC)
);
```

**When NOT to use:** When you want a single task instance, not all tasks from a spawn site. Use the task-ID recipe instead.

---

#### Recipe: Multi-trace union

**Question it answers:** "What does the aggregate CPU profile look like across multiple trace files?" Useful for comparing before/after or aggregating across replicas.

```js
const traceFiles = ['../traces/trace-a.bin', '../traces/trace-b.bin'];
const allSamples = [];
const mergedSymbols = new Map();

for (const url of traceFiles) {
  const buf = await fetch(url).then(r => r.arrayBuffer());
  const t = await TraceParser.parseTrace(buf);
  // Merge symbols (first-writer-wins per address)
  for (const [k, v] of t.callframeSymbols) {
    if (!mergedSymbols.has(k)) mergedSymbols.set(k, v);
  }
  allSamples.push(...t.cpuSamples.filter(s => s.source === 0));
}
const samples = allSamples;
// Use mergedSymbols instead of trace.callframeSymbols for setData()
FlamegraphRenderer.createFlamegraph(el, () => {}).setData(samples, mergedSymbols);
```

**Warning:** Timestamps come from different monotonic clocks across traces. Do NOT use time-based predicates (e.g., `s.timestamp >= X`) across traces in a union — the timestamps are incomparable. Cross-trace flamegraphs work because they aggregate by callchain, not time.

**When NOT to use:** When you need to compare traces side-by-side (differences). Use two separate flamegraphs instead.

## Linking back to the full viewer

The hosted viewer at `https://dial9-tokio-telemetry.netlify.app/` accepts these URL params:

| Param | Description |
|-------|-------------|
| `trace` | URL to the trace file (must be fetchable — absolute https URL) |
| `start` | Start of time range filter (monotonic ns) |
| `end` | End of time range filter (monotonic ns) |
| `svc` | Service name (display label) |
| `host` | Host name (display label) |
| `from` | Wall-clock start (ISO 8601, for display) |
| `to` | Wall-clock end (ISO 8601, for display) |
| `segs` | Comma-separated segment keys (for multi-segment traces) |

Canonical link template:

```html
<a href="https://dial9-tokio-telemetry.netlify.app/?trace=https://example.com/traces/full.bin&amp;start=3900000000&amp;end=4050000000"
   target="_blank">Open in dial9 viewer</a>
```

**Important constraints:**
- The netlify viewer fetches the trace via HTTP. It cannot load `file://` paths or relative paths from a local report. The trace must be hosted at a reachable URL (S3 presigned URL, public bucket, etc.).
- If you cannot host the trace, skip viewer deep-links and rely on `embed.html` (which loads via the local `dial9 report serve` server).
- **`?worker=` and `?task=` do NOT exist** as viewer URL params. Do not invent them.

## Slicing traces with `slice.js`

Slice traces to keep report folders small and embed loading fast.

**Important:** `--start`/`--end` are ABSOLUTE monotonic ns by default (matching `event.ts` from `parseTrace` — typically 10-15 digit numbers). Pass `--relative` if your numbers are offsets from trace start (typically 9-10 digit numbers like `3900000000` for 3.9s).

```bash
# slice the burst window (3.9s–4.05s into the trace):
node /path/to/dial9-trace-format/js/slice.js \
  --input full-trace.bin \
  --output report/traces/burst.bin \
  --relative \
  --start 3900000000 \
  --end 4050000000
```

Or programmatically:

```javascript
const { sliceTrace } = require('/path/to/dial9-trace-format/js/slice.js');
const fs = require('fs');

const input = fs.readFileSync('full-trace.bin');
const sliced = sliceTrace(input, {
  timeRange: { startNs: '3900000000', endNs: '4050000000' },
  relative: true,
});
fs.writeFileSync('report/traces/burst.bin', sliced);
```

**Why slice?** A full trace can be 100+ MB. Slicing to the relevant window (typically a few seconds) produces files of 100 KB–2 MB, making the report folder portable and embeds load instantly. The slicer preserves symbol table entries, segment metadata, and clock sync events regardless of the time range, so flamegraphs in sliced traces render with full function names.

Note: `slice.js` v1 supports `timeRange` filtering only. Event-type filtering is planned for a future release.

## Source-code links

For **public crates**, link to docs.rs:

```
https://docs.rs/hyper/latest/hyper/proto/h1/io/struct.Buffered.html
```

The `trace_parser.js` module exports a `_docsRsUrl(location)` helper that can generate docs.rs URLs from source locations like `hyper-0.14.28/src/proto/h1/dispatch.rs:174`.

For **private code**, ask the user for an example source link (e.g., `https://gitlab.acme.corp/team/repo/-/blob/main/src/foo.rs#L42`) and derive the URL template from it.

## Complete worked example

```html
<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="UTF-8">
<title>dial9 Trace Report — my-service</title>
<style>
:root { --bg: #0d1117; --surface: #161b22; --border: #30363d; --text: #e6edf3; --muted: #8b949e; --accent: #58a6ff; --critical: #f85149; --warning: #d29922; --info: #58a6ff; }
* { margin: 0; padding: 0; box-sizing: border-box; }
body { font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', Helvetica, Arial, sans-serif; background: var(--bg); color: var(--text); line-height: 1.6; padding: 2rem; max-width: 1100px; margin: 0 auto; }
h1 { font-size: 1.8rem; margin-bottom: 0.5rem; }
h2 { font-size: 1.4rem; margin: 2rem 0 1rem; border-bottom: 1px solid var(--border); padding-bottom: 0.5rem; }
.meta { color: var(--muted); font-size: 0.9rem; margin-bottom: 2rem; }
.finding { background: var(--surface); border: 1px solid var(--border); border-radius: 8px; padding: 1.5rem; margin-bottom: 1.5rem; }
.finding-header { display: flex; align-items: center; gap: 0.75rem; margin-bottom: 1rem; }
.severity { font-size: 0.75rem; font-weight: 600; text-transform: uppercase; padding: 0.2rem 0.6rem; border-radius: 4px; }
.severity-critical { background: var(--critical); color: #fff; }
.severity-warning { background: var(--warning); color: #000; }
.severity-info { background: var(--info); color: #000; }
.code-ref { font-family: 'SF Mono', monospace; font-size: 0.85rem; background: #1c2128; padding: 0.15rem 0.4rem; border-radius: 3px; }
a { color: var(--accent); text-decoration: none; }
a:hover { text-decoration: underline; }
</style>
</head>
<body>

<h1>dial9 Trace Report</h1>
<p class="meta">Service: <strong>my-service</strong> | Duration: 4.2s | Workers: 2</p>

<h2>Findings</h2>

<div class="finding">
  <div class="finding-header">
    <span class="severity severity-critical">Critical</span>
    <h3>Blocking file I/O on async worker at startup</h3>
  </div>
  <p>Task 3 performs a synchronous file read that blocks worker 1 for 17.7ms at t=350µs.
     Source: <span class="code-ref">main.rs:260:14</span></p>
  <p><strong>Fix:</strong> Move config loading to <code>spawn_blocking</code>.</p>
  <iframe src="embed.html?trace=traces/startup-io.bin&amp;view=flamegraph&amp;height=280"
          width="100%" height="280" style="border:0"></iframe>
</div>

<div class="finding">
  <div class="finding-header">
    <span class="severity severity-warning">Warning</span>
    <h3>Connection burst saturates workers (23ms scheduling delay)</h3>
  </div>
  <p>~80 tasks woken simultaneously at t≈3.9s cause wake-to-poll delays up to 23ms.</p>
  <p><strong>Fix:</strong> Increase worker count or add connection backpressure.</p>
  <iframe src="embed.html?trace=traces/burst.bin&amp;start=3900000000&amp;end=4050000000&amp;view=timeline&amp;height=120"
          width="100%" height="120" style="border:0"></iframe>
</div>

<div class="finding">
  <div class="finding-header">
    <span class="severity severity-info">Info</span>
    <h3>Memory allocation dominated by hyper read buffers</h3>
  </div>
  <p>728 sampled allocations (~403 MB throughput). Dominant site:
     <span class="code-ref">hyper::proto::h1::io::Buffered::poll_read_from_io</span>.
     6 allocations not freed — likely one-time startup allocs, not leaks.</p>
</div>

</body>
</html>
```

## What NOT to do

- **Don't inline trace bytes as base64.** Traces are megabytes; use sliced `.bin` files in `traces/`.
- **Don't render flamegraphs from scratch with CSS bars.** Use `embed.html` — it produces real interactive flamegraphs.
- **Don't use `embed.html?view=flamegraph` when the question is about a specific task, poll, or stack.** Use the standalone HTML pattern with a JS filter instead (see [Flamegraphs: precise filtering via standalone HTML](#flamegraphs-precise-filtering-via-standalone-html)).
- **Don't invent viewer URL params.** Only `trace`, `start`, `end`, `svc`, `host`, `from`, `to`, `segs` exist. There is no `?worker=`, `?task=`, or `?source=`.
- **Don't copy the full multi-MB trace into the report folder.** Slice it to the relevant time window.
- **Don't rely on `file://`.** Reports embed iframes that fetch trace files. Tell users to view via `dial9 report serve <folder>` (or `python3 -m http.server`).
