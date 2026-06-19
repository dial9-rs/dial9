---
name: dial9-zoom-window
description: Zoom into a narrow time window of a dial9 trace to see every worker and OS thread at one moment. Use after an aggregate pass (`analyze.js`, `red_flag_scan.js`) flags a timestamp — a long poll, a queue spike, a latency outlier. Use when the user says "zoom in", "what was happening at +6953ms", or "show me the window around that poll".
---

# Zooming into a time window

Aggregate analysis averages over the whole trace; it cannot show what was
happening **around** a single event. Pick a timestamp and reconstruct the
instant: which polls ran on which workers, who was on-CPU, whether the queue
was backed up.

Pair with `dial9-diagnose-long-poll` (applies this windowing to root-cause one
poll) and `dial9-runtime` (the execution model).

## When to zoom

Zoom **after** an aggregate pass has given you a timestamp of interest:

- A long poll from `red_flag_scan.js` (`Poll of 53.1ms ... at 6953.5ms`) — but note
  that "long" is relative: judge a poll by how far it sits in *this runtime's own*
  poll-duration tail (`pollDurationByLoc` p99), not an absolute millisecond cutoff.
  See `dial9-diagnose-long-poll` for the calibration.
- A scheduling-delay outlier or queue-depth spike
- A `pollDurationByLoc` max you want to see in context
- A specific `request_id` whose spans you've located in time

Do **not** zoom to go fishing in a healthy trace — you'll drown in microsecond
polls. Start aggregate, then zoom to the timestamp the aggregate flagged.

## The mechanics: timestamps

dial9 timestamps are nanoseconds. Every tool reports time **relative to the start
of the trace in milliseconds** (`+6953.5ms`). To convert a relative-ms position
to an absolute timestamp:

```
absoluteNs = trace.minTs + relMs * 1e6
```

A window is `[center - half, center + half]`. Pick `half` to be a few times the
duration of the event you're studying: for a 53ms poll, a ±40ms half-window shows
the poll plus what bracketed it on both sides.

## Run it

```bash
node scripts/zoom.js <trace.bin|dir> <centerMs> [halfWindowMs=20]
```

`centerMs` is the relative-ms timestamp from the aggregate tools.

Example — zoom on a 53ms long poll the red-flag scan reported at +6953ms:

```bash
node scripts/zoom.js /tmp/traces/host3/trace.bin.gz 6980 40
```

## What it shows, and how to read each section

### 1. Cross-worker poll timeline

Every poll overlapping the window, **across all workers**, ordered by start time.
`*` marks polls > 1ms, annotated with on-CPU / off-CPU. This is the "what ran
when" view — read it top to bottom like a tape.

What to look for:
- A single `*` poll that dwarfs everything else → your event. Note its worker.
- A burst of polls for one `task=` → a hot connection or a tight wake loop.
- A gap with no polls → the runtime was parked (nothing to do) or everything was
  blocked inside one long poll.

### 2. Per-worker busy time

How many ms of the window each worker spent inside polls. One worker at ~100% and
the rest near 0% means the work is **not** parallelized across the window —
either one task is hogging a worker, or the box is nearly idle with one straggler.

### 3. Per-OS-thread CPU census  ← the section people forget

CPU samples in the window grouped **by OS thread (tid)**, with an estimate of
on-CPU time. This is the most important and least obvious view:

> **perf samples a thread only while it is ON-CPU.** A parked/blocked thread
> produces **zero** samples. So this census is a census of *who was actually
> running*, regardless of what the tokio scheduler thought.

At the 99 Hz default, one sample ≈ **10ms of on-CPU time** for that thread. So:

- **5 samples on one tid over a 50ms window ≈ that tid was on-CPU the whole time.**
- `gaps~10.0ms` (evenly spaced) confirms it was continuously running, not bursty.
- A tid here that is **not** a tokio worker (e.g. a `spawn_blocking` pool thread,
  a metrics flusher, the allocator) is often the hidden actor behind a stall.
- **No samples at all** in the window → the whole box was idle; any long poll in
  the window is blocked off-box (network/disk), not on local CPU or a local lock.

Do not reason from the *aggregate* sample rate ("p50 gap was 70ms so the box was
idle") — aggregate gaps are large precisely because few threads are on-CPU. Always
reason **per tid**.

### 4. Global queue depth

Max/min global injection-queue depth in the window. `max=0` while a worker sits in
a 53ms poll is the proof that the long poll was **not** causing head-of-line
blocking — nothing was waiting behind it. `max>0` means work piled up: the long
poll (or a busy runtime) was actually delaying other tasks.

### 5. Inner spans of the dominant poll

If the app installed the tracing layer (`Dial9TokioLayer`), the `tracing` spans
that executed inside the longest poll, by name. Tells you *what code* ran inside a
long poll even without reading CPU stacks.

## From window back to cause

The zoom is descriptive — it shows the instant. To turn "worker 9 sat in a 53ms
off-CPU poll while tid=31 ran `EmfCollector::flush` the whole time" into a root
cause and a fix, hand off to **`dial9-diagnose-long-poll`**, which formalizes the
per-tid census into a holder-vs-off-box verdict and corroborates it across the
trace.
