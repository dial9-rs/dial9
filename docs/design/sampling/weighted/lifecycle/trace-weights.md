Implemented only the new sampler module `dial9-sampling/src/trace_weights.rs`; root owns module declaration, startup/worker integration and registration wiring.

Private API:
- `TraceWeights::new(pool: String, incarnation: String, initial_weight: f64) -> Self` (finite positive startup invariant).
- `multiply(&self, factor: f64) -> Result<(), StoreError>`: appends a synchronous source-clock survivor transition. Numerical overflow/underflow marks an explicit unknown/null interval and returns an error. Multiplication after terminal zero/unknown returns an error without making a plausible weight.
- `stop(&self)`: idempotently appends synchronous known zero, defining exclusion after the local terminal decision before asynchronous actual recorder disable catches up.
- `current_expansion_weight(&self) -> Option<f64>`: Some(0) is terminal exclusion; None is numerical unknown.
- `attach(&self, &Recorder) -> bool`: registers by source type, is idempotent for the same shared state, refuses a different invocation's existing source, and logs failures. No recorder references are retained.

Metadata namespace `dial9.sampling`: schema=1, unit=trace_emitting_unit, clock=monotonic_ns, pool, incarnation, weight_history JSON [[timestamp_ns, weight_or_null], ...], weight_known, observed_through_ns. The source snapshots history and checkpoint inside the update mutex. Static/history fields change only with the timeline; the checkpoint changes each metadata flush. Core's merged metadata repeats full history in subsequent segments. Readers use the last chronological transition at/before each frame timestamp, with coverage only between first transition and checkpoint. A later segment may extend earlier coverage. Never apply current weight to every frame in a file. Zero excludes post-decision lag frames; null is unknown.

Validation:
- cargo nextest run -p dial9-sampling --features dial9 -E 'test(trace_weights)': four tests passed.
- Real disk-recorder encode/decode test: one sealed segment correctly assigns weights 10 to older frames, 20 to post-survival frames, and 0 to frames collected after a terminal decision before actual recorder disable.
- Tests cover cumulative full history, checkpoint monotonicity, overflow/null with earlier history retained, duplicate stops and forbidden resume, idempotent attachment and conflicting/disabled attachment.
- cargo clippy -p dial9-sampling --all-targets --features dial9 -- -D warnings: passed.
- New Shuttle survivor/terminal update vs metadata checkpoint race: PCT and determinism passed. Existing three core missing-Debug warnings remain unchanged.

Root integration cautions already reported:
- Combine registered StopAction with weights.stop for enabled fallback invocations lacking a worker session, as well as keep the worker's earlier failed-coin stop.
- Consider terminal disable on a false attach result so conflicting recorder metadata cannot silently stand in for this invocation.
- Core discards metadata-only sealed segments. A truly idle recorder may lack uploaded new checkpoints; later real segments can extend its history. Coverage must remain bounded by available evidence, especially final source events with timestamps newer than that cycle's checkpoint.
