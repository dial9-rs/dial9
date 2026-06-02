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
- **Don't invent viewer URL params.** Only `trace`, `start`, `end`, `svc`, `host`, `from`, `to`, `segs` exist. There is no `?worker=`, `?task=`, or `?source=`.
- **Don't copy the full multi-MB trace into the report folder.** Slice it to the relevant time window.
- **Don't rely on `file://`.** Reports embed iframes that fetch trace files. Tell users to view via `dial9 report serve <folder>` (or `python3 -m http.server`).
