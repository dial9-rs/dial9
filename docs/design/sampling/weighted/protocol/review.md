# Protocol and storage review

Reviewed `/tmp/dial9-manual-sampling-spike` at `95b4639cd015d52d5e0e30d5cd93dc359bd29036`. Scope: storage semantics, election/CAS fencing, clock/bucket edges, pagination, census liveness/cost, and historical-estimation implications. No author files were changed.

## Confirmed findings

### P1 — Historical population slices are not recoverable from the current storage layout

Source: `/tmp/dial9-manual-sampling-spike/dial9-sampling/src/protocol/engine.rs:723` (every publication overwrites `roster.json`), `:673` (membership bodies are empty), `:1026` (shutdown deletes unique membership objects).

Reachable trigger: run a fleet, publish multiple censuses, terminate its processes cleanly, then perform analysis of a past time slice. Without independently enabled bucket versioning or external capture, earlier census values have been overwritten and membership keys have been removed. Versioning is not required or consulted by this API.

Consequence: the new user requirement cannot be met from sampler storage as implemented. Two fleets with different populations at an earlier time can have the same final roster and surviving membership, so an offline reader cannot infer the earlier population.

Minimal fix: preserve immutable, time-stamped census observations independently of the mutable leadership object. Persist the pool/policy identity, tracked count, tracking probability, census time/window, and observation quality/freshness. Specify the achievable time resolution. This is sufficient to reconstruct estimates of the observed roster membership; see the separate time-slice caveat below before labeling it an instantaneous live-host estimate. Do not rely on optional S3 versioning as an implicit requirement.

This is a gap against the newly stated requirement, rather than a regression against the prior live-admission contract.

### P2 — Accepted short-bucket configurations can never produce a census on native S3

Source: `/tmp/dial9-manual-sampling-spike/dial9-sampling/src/protocol/mod.rs:121` validates bucket width against configured host skew but not backend timestamp uncertainty. `/tmp/dial9-manual-sampling-spike/dial9-sampling/src/protocol/engine.rs:795` includes that uncertainty in the actual census guard; `:802` requires a phase strictly before the upper edge; `:809` logs an impossible safe window and returns a polling cadence. `/tmp/dial9-manual-sampling-spike/dial9-sampling/src/s3.rs:39` declares one second of uncertainty.

Reachable trigger: `poll_interval = 100ms`, `bucket_width = 1s`, `max_clock_skew = 0`, and native S3. The config passes validation. The effective guard is 1s at each edge, so no phase of a 1s bucket can satisfy the census predicate. A less extreme accepted example is poll=1s, bucket=10s, skew=2s: effective guard=5s, and the strict upper comparison still leaves zero safe phases.

Consequence: the elected worker keeps refreshing membership and renewing the shared lease, but never lists/publishes a census or settles admission. With the disabled failure policy, future processes remain permanently off; with an assumed-population policy, no trimming census is available. The default 5-minute bucket is unaffected.

Minimal fix: perform backend-aware timing validation after backend construction and before starting maintenance, requiring `bucket_width > 2 * (2 * max_clock_skew + modification_time_uncertainty)` at minimum. Return an explicit invalid configuration/failure decision instead of maintaining a policy that cannot progress. Add a regression with a backend that declares S3-like 1s uncertainty; the existing short-bucket tests use an exact-time in-memory backend.

Evidence: `/tmp/dial9-sampling-review-protocol/window-reproduction.py` translates the exact integer timing predicates. Its evaluated cases are in `window-reproduction.json`; both accepted examples have zero safe phases.

## Important analysis/model caveats

### Current population is observed process membership, with a separate lag/ghost error

Source: `/tmp/dial9-manual-sampling-spike/dial9-sampling/src/protocol/engine.rs:514` lists only current/previous buckets, `:523` deduplicates by process incarnation, and `:590` multiplies that count by the tracking factor. Listed modification times are not used to estimate exact lifecycle intervals.

A process that exits or crashes while cleanup fails remains represented until its last bucket leaves the two-bucket window, potentially almost two bucket widths. An incomplete census can undercount; failed cleanup can overcount. These effects are not Gaussian hash-sampling error and can dominate it for rapidly turning over fleets. Archiving census snapshots preserves the biased membership estimate, not an exact instantaneous live count. A visualization should show this lag/bias separately from the bell curve.

Processes/containers are the counted unit. Simultaneously live incarnations on one physical machine count separately. A physical-host estimate needs a separately defined stable physical-host identity and deduplication policy; reusing `host_id` for multiple simultaneously live samplers currently also violates the independent-rank assumption documented by the API.

### Active census membership is admission, not verified recording

Source: `/tmp/dial9-manual-sampling-spike/dial9-sampling/src/protocol/engine.rs:637` sets `Active` immediately from the startup choice, before the application callback creates or registers a recorder. The application can delay registration or create none.

The visualization's “actually ran profiling” metric should come from explicit simulated recorder lifecycle state, and post hoc product telemetry should use recorder/trace evidence. `counts.active` alone cannot establish it.

### Election bursts are bounded per contender, but current cost evidence is optimistic

Source: `/tmp/dial9-manual-sampling-spike/dial9-sampling/src/protocol/engine.rs:751` spreads first election attempts over at most two seconds and rereads before CAS. There is no slot-probe/retry loop; each contender performs at most one claim in that election operation.

Close races can still produce one losing CAS per tracked process, especially with realistic network latency and a dense cold start. I did not find a correctness bug here. The evidence branch's request-count simulation assumes unbounded concurrency and a 1ms illustrative read-to-CAS interval, so it cannot establish a tight burst-rate/latency bound for real S3. A cost simulator should vary service latency/concurrency and show attempts, conflicts, and total operations.

## Checks that looked sound

- Leadership and census publication use the same conditional object, and each mutation uses a fresh revision nonce; paused leaders fail after an actual takeover changes the version.
- Native S3 conditional writes disable SDK retry hiding, and uncertain outcomes reconcile through a subsequent strong GET before owned publication resumes.
- Complete unordered pagination is enumerated; failed pages and repeated continuation tokens fail the census rather than publishing partial counts.
- Membership cleanup targets fresh incarnation-owned keys, so an old invocation cannot delete a replacement invocation's membership.
- Carrier state wins across bucket versions, and recording does not resume.
- Trimming checks both server modification-time lease age and embedded census age, protecting against delayed old censuses receiving fresh server write timestamps.
- Known policy mismatch does not join the census or rewrite the incompatible roster.

## Validation

`cargo nextest run -p dial9-sampling --all-features`: **95 passed, 0 skipped** on the reviewed source. This is a focused review run, not a claim to have repeated the complete workspace/stress/Shuttle validation. The liveness reproduction is analytical and does not require author-file modifications.
