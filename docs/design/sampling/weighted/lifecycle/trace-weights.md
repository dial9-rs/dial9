# Evaluated scalar trace weights

This supersedes the timestamped-history source in implementation `1904be0b`.
The current revision is pinned in `../validation.json`.

`TraceWeights` owns a pool key and one shared `Option<f64>`. An initially selected
process starts at `1/(f*r)`. Each independently survived round multiplies the
current value by `1/q` once; overflow becomes unknown and terminal exclusion
sets zero. The attached `Source` snapshots that value under the update mutex
when `segment_metadata` is evaluated. Unchanged snapshots emit no update.

The emitted metadata is only `dial9.sampling.pool` and `dial9.sampling.weight`.
Finite values are numeric strings; `null` means unknown, while zero means
stopped at evaluation. Identity comes from the existing `boot_id`; clock data
comes from normal trace events. Core's merged metadata carries its latest
captured snapshot into rotated segments. No history, checkpoint, clock label,
unit label, additional trace identity, schema key, or known flag is emitted.

A decoder can apply the recorded scalar to a segment's sample mass. It is correct
at metadata evaluation, and approximates changes during the segment. It does
not recover exact recording-time factors for arbitrary older frames. The source
adds no segment-boundary synchronization or independent metadata heartbeat.

Tests cover immutable prior snapshots, fresh values after updates, unchanged
emission suppression, overflow/null, terminal zero and forbidden resume,
idempotent/conflicting attachment, and weak recorder ownership. A real encoded
and decoded segment captures weight 20 after an update from 10, with exactly two
sampling keys, the existing boot ID, and a normal clock-sync event. Shuttle checks
survivor/terminal updates racing metadata evaluation.

Historical census observations still serve retrospective population queries,
including processes whose profiling choice was off. They are a separate input;
the source's scalar does not need a full roster embedded in the trace.
