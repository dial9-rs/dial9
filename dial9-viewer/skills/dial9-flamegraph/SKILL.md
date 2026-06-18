---
name: dial9-flamegraph
description: Render an SVG (or speedscope) flamegraph of on-CPU time from dial9 trace segments, outside the viewer. Use when an agent or CI needs a standalone flamegraph file from .bin.gz traces, wants to aggregate an arbitrary union of segments (across hosts/times), or needs folded stacks for custom analysis. For the interactive in-browser flamegraph use the viewer; for comparing two profiles use dial9-flamegraph-diff.
---

# dial9 → standalone flamegraph

The dial9 viewer renders flamegraphs interactively in the browser. This skill is
the **headless** path: turn trace segments into a self-contained flamegraph file
(or folded stacks) from the command line — for agents, reports, CI artifacts, or
aggregating an arbitrary set of segments the viewer's time-window model can't
express (e.g. "all 6 hosts × 10 spread-out minutes").

It profiles **on-CPU** time (`cpuSample.source === 0`) with fully symbolized
Rust/Tokio stacks.

## The pivot: folded stacks

Everything here goes through **folded** format — one line per unique stack:

```
<root>;<caller>;...;<leaf> <count>
```

Folded is the lingua franca of flamegraph tooling (flamegraph.pl, inferno,
speedscope all read it), and it's also what `dial9-flamegraph-diff` and any
custom stack analysis consume. `fold.js` is the only dial9-specific step; after
that you're in standard-tooling land.

## Prerequisites

1. **dial9 toolkit** (provides `trace_parser.js` + `decode.js`, which `fold.js` reuses):
   ```bash
   dial9 agents skills /tmp/d9-skills       # unpack all skills incl. dial9-toolkit + this one
   node /tmp/d9-skills/dial9-flamegraph/scripts/fold.js <trace>
   ```
   `fold.js` resolves the parser as a sibling first, then from the `dial9-toolkit`
   skill's `scripts/`, then `ui/` — so running it from an unpacked skills dir (or
   the source tree) works without copying files around.
2. **A renderer.** Either:
   - **inferno** (Rust, no browser): `cargo binstall inferno` → `inferno-flamegraph`.
   - or **speedscope** (`https://speedscope.app`, drag in a folded file), or the
     classic `flamegraph.pl`. All read the same folded output.
3. **Trace segments** — `.bin.gz` files. From S3 see `dial9-s3-analysis` for
   discovery/download (and `dial9 agents skill dial9-s3-analysis`). Each segment
   is ~one host-minute; parsing is ~10s/segment (CPU-bound).

## Workflow

### 1. Fold segments into stacks
```bash
# a file, several files, or a whole directory:
node fold.js /tmp/d9-traces/seg1.bin.gz /tmp/d9-traces/seg2.bin.gz > profile.folded
node fold.js /tmp/d9-traces/                                        > profile.folded
```
Flags:
- (default) full symbolized stack, root→leaf.
- `--max-depth N` — keep only the N frames nearest the leaf (smaller, coarser).
- `--leaf` — collapse to leaf symbol only (a 1-deep hotspot histogram).
- `--json` — emit `{stack: count}` JSON instead of folded text.

**Aggregating many segments / many renders:** fold each segment once to `--json`
(parallelize with `xargs -P`), then merge subsets cheaply without re-parsing:
```bash
ls /tmp/d9-traces/*.bin.gz | xargs -P 8 -I{} sh -c 'node fold.js --json "{}" > "{}.json"'
node merge_folded.js /tmp/d9-traces/*.json > pool.folded
```

### 2. Render
```bash
inferno-flamegraph --title "Shale on-CPU (60 host-minutes)" --colors rust \
  profile.folded > profile.svg
```
Useful flags: `--colors rust|js|java`, `--reverse` (icicle / leaf-rooted, good for
shared leaf hotspots), `--minwidth 0.5` (drop slivers). Or open the folded file in
speedscope for an interactive view.

### 3. Deliver
Write the `.svg` somewhere durable (not a temp dir). It's self-contained and
interactive (click to zoom, Ctrl-F to search) in any browser.

## Gotchas

- **On-CPU only.** Off-CPU/blocking time needs schedule profiling enabled at
  capture (`DIAL9_SCHEDULE_PROFILE_ENABLED=true`); without it, parked/blocked
  time won't appear. Don't describe the result as wall-clock.
- **`;` in symbols** would corrupt folded format — `fold.js` already maps them to `:`.
- **`0x…` frames** = unsymbolized addresses (a few leaf/JIT/vsyscall frames); normal.
- Parsing dominates runtime; cache with `--json` and reuse.

## Related
- `dial9-flamegraph-diff` — compare two profiles (host vs host, time vs time, node vs fleet).
- `dial9-s3-analysis` — discover/download trace segments from S3.
- `dial9-toolkit` — the parser/analysis JS this builds on.
