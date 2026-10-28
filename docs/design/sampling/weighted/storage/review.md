# Weighted sampling storage/control review

Reviewed the working changes in `/tmp/dial9-manual-sampling-spike` after reading its `AGENTS.md`, sampler README and build configuration. Scope: schema-3 roster fencing, archival history, fresh admission and trim draws, trim epochs, per-frame trace weighting, and zero-support evidence. This is an independent read-only review; no production files or commits changed.

## Outcome

No remaining implementation blocker found in this review. The protocol and weighting contracts are substantially more defensible than sorted rank trimming for reconstructing a weighted fleet workload. Claims remain conditional on independent unit identities and the visibility/coverage assumptions documented by the API.

## Findings

1. **P2 documentation mismatch, communicated to the owner.** The README still describes fallback selection as "by rank" and says a fresh census "can trim active processes to target + max_overshoot". The implementation uses independent invocation admission after sparse census eligibility, and fresh salted Bernoulli thinning with survival probability `target / observed_active` when the soft threshold is exceeded. Its result is a random count around the target; the threshold is not a hard post-trim count guarantee. README should describe this mechanism and the timestamped per-frame weight history, including cohorts with zero profiling probability.

2. **Focused regression coverage opportunity, not a demonstrated defect.** Add an actual Worker survivor test asserting that repeated observation of the same epoch never multiplies weight twice, a new Worker skips the already-published round, and a later fresh epoch multiplies exactly once. The guards appear correct by inspection and the independent small model; present protocol tests demonstrate terminal decisions and immutable startup status, while direct trace-weight tests verify histories but do not explicitly assert these combined survivor weight values.

## Protocol observations

- Schema-3 policy matching prevents a process from treating an old sorted-rank roster as a weighted Bernoulli roster. Fresh revisions keep body-based ETags from repeating; a combined leadership/roster CAS fences an old paused publisher after takeover.
- The admission random draw is domain-separated from census eligibility. The startup probability is frozen and recorded rather than replaced afterward with a current population estimate. Stable identities may intentionally share census membership across restart observations, but simultaneously live units require independent identities for the advertised census errors.
- Trim thresholds are derived from an observed cohort before a fresh salt is chosen. Workers remember the last applied epoch; startup initializes that epoch from the roster already read, and terminal carriers never apply later rounds or resume. A surviving unit multiplies its expansion weight by the inverse of that round's survival probability at its local monotonic transition time.
- Old census age is checked independently of storage LastModified, so a delayed server commit does not make an expired measurement suitable for trimming. Cooldown limits rapid repeated rounds; it does not certify that every follower applied a round or impose an unconditional cap during partitions.
- Terminal weight zero is recorded before stop notification. Analysis must select the weight at each frame timestamp and reject coverage beyond its checkpoint; applying the current segment weight retroactively would be wrong. Weight overflow becomes explicit unknown evidence instead of a plausible finite fallback.
- Immutable census observations are written before the roster CAS. A subsequent failed or unknown CAS does not invalidate that completed enumeration; query results correctly identify historical observations rather than assert that their policy changes were committed. Archive failure prevents roster publication, and uncertain writes require subsequent reconciliation rather than a blind conditional retry.
- Historical lookups respect requested time and age, require complete pagination, and reject malformed/missing evidence. Gaussian sampling uncertainty covers visible membership only. Bucket ghosts, short-lived missed units, absent participants and clock violations are distinct coverage bias.
- `no_support` counts tracked units admitted under probability zero. This exposes some otherwise unreconstructable cohorts; it does not prove that all missing or partitioned units have been counted. Positive inclusion probability and known per-frame weights are necessary to reconstruct fleet workload from profiling traces.

## Independent checks

- `cargo nextest run -p dial9-sampling --features dial9 -E 'test(protocol::engine::tests) or test(history::tests) or test(trace_weights::tests)' --no-fail-fast`: **43 passed, 31 skipped**, including actual stale CAS, unknown acknowledgements, bucket guards, terminal recorder decisions, history persistence and real emitted segment metadata. Owner is running broader final validation.
- `exact_moments.py`: exact rational enumeration of **108** scenarios with populations 1..6, startup probabilities 1, 1/2 and 1/4, targets 1 and 2, and 0..2 adaptive `q = min(1, target / active)` rounds. Expected expanded totals equal the true population exactly; all probability masses equal one. **18** additional sparse-census/admission split cases also have exact population expectation. This is an ideal complete-participation model, not a bound against missing storage coverage or zero-probability cohorts.
- `control_model.py`: independent abstract exploration of two workers, three roster mutations, delayed stale CAS attempts, repeated read delivery and 64 fixed round-draw assignments. **627,396 reachable states**, **1,731,672 transitions**, including **504,672 stale-CAS transitions**, preserve once-only application, terminal no-resume and startup skip of pre-existing rounds. The model checks the control guards, not every Rust/storage/timing interleaving.

All review and simulation resources are separate from the implementation PR.
