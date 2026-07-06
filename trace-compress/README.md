# dial9-trace-compress

Experimental **format-aware compressor** for dial9 trace files. It parses the
trace wire format, re-arranges the data into homogeneous streams with
per-stream transforms, and entropy-codes the result with zstd. Decompression
reconstructs the original file **byte-exactly** (verified on every `bench`
run and in the test suite).

## Results (demo trace, 11,081,465 raw bytes)

| method | size | ratio | compress time |
|---|---:|---:|---:|
| gzip -6 (current demo-trace.bin) | 3,363,929 | 30.4% | ~360 ms |
| gzip -9 | 3,307,979 | 29.9% | ~970 ms |
| zstd -19 (raw bytes) | 2,675,025 | 24.1% | ~2.8 s |
| **d9tc (default, level 12)** | **1,511,317** | **13.6%** | **~290 ms** |
| d9tc level 19 | ~1,460,000 | 13.2% | ~650 ms |

At the default level the output is **2.2× smaller than gzip -6 while
compressing ~25% faster**; even the raw file run through `zstd -19` is 1.8×
larger. Decompression is ~68 ms. Peak encoder memory is ~100 MB, dominated by
zstd's high-level match-finder tables (gzip streams in ~2 MB); decode peaks
at ~28 MB. The tool is not streaming: it holds the input and the rearranged
streams in memory.

## How it works

The trace format ([`dial9-trace-format`]) is a frame stream: schema frames,
event frames (u16 type id + u24 timestamp delta + schema-driven fields),
string/stack pool frames, and timestamp resets, concatenated across segments
that each restart with a `TRC\0` header. The compressor tokenizes frames and
splits them into streams that each look "like one thing" to the entropy coder:

- **Frame sequence** — one varint symbol per frame. This absorbs the 6-byte
  per-event header (~3.1 MB of the demo trace) into a highly repetitive
  stream that zstd crushes ~15:1.
- **Timestamp deltas** — one stream per event type (gap distributions are
  strongly conditioned on the type), varint-coded. Reset values are
  delta-coded against the previous reset.
- **Field columns** — keyed by *schema identity* (FNV-1a of the schema
  definition, not the wire type id, which is only stable within a segment)
  and field index. Per-column transforms:
  - `span_id`/`parent_span_id` varint columns: a global recent-id cache
    encodes repeats (enter/exit/close of the same span) as back-distances;
  - pooled-string / u32 / tid fields: varint instead of fixed width;
  - optional fields: presence bytes in their own stream;
  - blob fields: lengths and payloads split.
- **String pool** — ids as deltas (all 1s), lengths, and data split;
  canonical lowercase hex UUIDs (92% of the demo pool) are nibble-packed to
  16 bytes.
- **Stack pool** — addresses zigzag-delta coded within each stack, first
  frame delta'd against the previous stack's first frame.

The streams are grouped into two zstd frames: a *structured* group whose
level is the speed knob (`DEFAULT_LEVEL` = 12), and a small *entropy-bound*
group (timestamp gaps, packed UUIDs, reset jumps) always compressed at level
19 — measured, that group repays a high level far better per millisecond
than the structured group.

An adaptive per-column zigzag-delta mode was tried and removed: it bought
only ~14 KB (0.9%) on the demo trace and was the most stateful part of both
codec sides.

Remaining size is dominated by genuinely random content: inter-event gap
entropy (~530 KB) and packed UUID bytes (~400 KB).

## Usage

```
cargo run --release -p dial9-trace-compress -- bench <raw-trace.bin>
cargo run --release -p dial9-trace-compress -- compress <raw.bin> <out.d9tc> [level]
cargo run --release -p dial9-trace-compress -- decompress <in.d9tc> <out.bin>
cargo run --release -p dial9-trace-compress -- analyze <raw.bin>   # byte-budget report
cargo run --release -p dial9-trace-compress -- streams <raw.bin>   # per-stream attribution
```

Note `dial9-viewer/ui/demo-trace.bin` is gzipped; the raw trace input is
`gunzip -c dial9-viewer/ui/demo-trace.bin`.

## Status / caveats

- Experimental; the container format (`D9TC` v1) has no stability promise.
- Byte-exact reconstruction relies on the trace encoder emitting canonical
  (minimal) LEB128, which `dial9-trace-format` does. The round-trip is
  asserted in tests; a mismatch would surface as a failed equality check,
  not silent corruption.
- Not wired into the viewer or any production path. A JS decoder port (the
  viewer's decoder-of-record) would be a few hundred lines mirroring
  `decompress()` plus a zstd WASM/JS dependency.
- **Tuning corpus**: transforms were designed against the demo trace.
  Correctness is guarded independently by `tests/torture.rs` (hand-built
  traces covering every field type, optionals with nonstandard presence
  bytes, wire type ids remapped across segments, pool/reset edge cases) and
  verified on separately captured traces. Ratio claims, however, are
  demo-trace numbers; the UUID packing and span-id cache only pay off on
  workloads that have those shapes (both degrade gracefully to pass-through).
- Small inputs (< a few KB) can come out larger than gzip: the stream
  directory and dual zstd frame headers are fixed overhead.
