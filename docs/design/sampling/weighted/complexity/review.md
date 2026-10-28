# Sampling implementation complexity review

Reviewed `1904be0b31d3bb49960afe2de4ae9b51f5ab8540` read-only. Line references below refer to that commit. Root is already replacing the trace timeline with a current scalar metadata source; no production changes or ticket comments were made by this reviewer.

The remaining protocol complexity mostly serves real invariants. Four small simplifications are worth doing while this API is unpublished.

1. **Remove remnants of the old rank-boundary algorithm and derived wire diagnostics.** `protocol/engine.rs:166` defines `Boundary { rank, incarnation }`, but there is no boundary ordering operation anymore. It is only nested inside `Member` (`:495`); flatten to `rank` and `invocation_id`. Keep the unique invocation ID internally: it supplies independent admission/thinning entropy, separates overlapping startups, and makes membership cleanup safe before a recorder/boot ID exists. It need not appear as another trace identity.

   `Roster.population` and `slice_too_small` (`:224`) are purely derived diagnostics. Their only reads are debug logs (`:403`, `:1001`), plus fixture assignment and one assertion. Compute the diagnostics from counts/config when logging rather than requiring wire fields. A census timestamp can distinguish unknown from a successful empty count. Keep `settled` and `cutoff_settled`: these have separate control roles, respectively leader pass cadence and a persistent startup admission cutoff.

2. **Drop the unused timestamp from LIST results.** `backend.rs:217` requires `ListedObject.modified_at`, and `s3.rs:200` parses/rejects absent or negative LIST LastModified. Neither protocol membership enumeration nor historical lookup reads this timestamp; both consume keys only. This imposes an unused contract on every custom backend and tests response conditions that cannot affect the calculation.

   Keep `StoredObject.modified_at` from roster GET: lease expiration uses it. Keep full pagination, cyclic-cursor detection, prefix validation, and explicit failures; these are necessary to prevent partial censuses. Backend timestamp uncertainty also stays because roster lease and census timing guards use it.

3. **Use the existing unknown-write error for 409 conflicts.** `PutOutcome::RetryableConflict` (`backend.rs:153`) is immediately converted into `StoreErrorKind::WriteOutcomeUnknown` by both engine consumers (`engine.rs:828`, `:983`). `s3.rs:149` can return that error directly, eliminating the variant/constructor and duplicate match branches.

   This is representation cleanup only. Do not change retry suppression, interpret 409 as a definitive failed precondition, or bypass the subsequent GET reconciliation.

4. **Replace the mutable cleanup-budget channel with the fixed budget.** `SamplingSession.cleanup_budget` (`engine.rs:69`) is an `Arc<Mutex<Duration>>` updated by the private `shutdown(timeout)` method and read once by the worker. Public invocation cleanup always chooses 500ms (`lib.rs:499`), matching the worker's initial default (`engine.rs:423`). A shared constant removes synchronization and a private configuration knob. Tests currently pass 1s only to join shutdown; preserve bounded cancellation/cleanup and adjust them to the fixed contract.

One deeper simplification should be a separate change: `registration.rs:53` spawns a stop listener for every registered recorder and stores each JoinHandle. The single session's terminal watch event could instead close one callback registry through one listener. That would remove per-recorder task ownership and internal stop-watch plumbing, but it needs deliberate registration-versus-stop race coverage. It is not needed to complete the current metadata cleanup.

Do not remove the census archive simply because traces now carry scalar weights. The separate retrospective population requirement still needs retained evidence for processes whose profiling choice is off. Its explicit absence/staleness handling and sparse-count uncertainty are useful, rather than unnecessary precision.

Resolution: the four small cleanups above are implemented in the revision pinned
in `../validation.json`. The census archive is retained. The listener registry
refactor remains separate; there is no public stopped API.

A second read-only review of `1904be0b..6d9652ee` found no actionable semantic
regressions. The scalar snapshot preserves terminal zero, unknown overflow and
attachment ownership. Invocation keys, carrier deduplication, epoch handling,
CAS fencing, uncertainty reconciliation, pagination and GET timing remain
intact. Fixed cleanup is bounded in the worker and caller.

A subsequent lifecycle simplification removes the sampler-specific permanent
recorder state/callback and the one-shot callback wrapper. Sampling now calls
normal `disable()`; explicit application `enable()` overrides sampling. Cleanup
can call disable again after that override. The core crate matches the PR base.
See `../validation.json` for the implementation revision and checks.
