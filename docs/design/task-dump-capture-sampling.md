# Task Dump Capture Sampling and Mixed Flamegraphs

Status: proposed

## Summary

Task dumps currently capture an async stack after every instrumented poll that
returns `Pending`, then use an idle-time Poisson decision to choose which
captures to emit. Sampling only at emission limits trace volume, but it does
not limit the expensive stack captures.

Replace that behavior with one worker-local sampling decision before stack
capture:

```rust
TaskDumpConfig::builder()
    .captures_per_second_per_worker(10)
    .build()
```

Each worker independently targets the configured capture rate across eligible
pending transitions. A selected transition has no second emission-sampling
decision: if the `trace_with` re-poll remains pending, all captured callchains
are emitted immediately. Each `TaskDumpEvent` records its inclusion probability
so analysis can recover unbiased wait-time estimates.

The same statistical contract enables a task-scoped mixed flamegraph:

- on-CPU stacks are weighted from `cpu.profile.frequency_hz` segment metadata;
- async task dumps are weighted by observed idle duration divided by their
  inclusion probability.

Mixed flamegraphs are intentionally scoped to one task in the first version.
Process- or runtime-wide views would be biased when some tasks are not
task-dump instrumented: those tasks have zero inclusion probability, which no
weighting can correct.

Task scope applies only to the first mixed-flamegraph view. Capture itself is
not restricted to one task: every task wrapped by dial9 instrumentation on
every attached runtime worker participates in that worker's sampler.

## Goals

- Bound expected task-dump capture work as a simple function of runtime worker
  count.
- Sample eligible transitions from all dial9-instrumented tasks on all workers,
  without a task allow-list or a designated sampling worker.
- Put the sampling decision before `tokio::runtime::dump::trace_with`.
- Emit every usable selected capture; do not maintain a second emission
  sampler.
- Preserve enough sampling information for unbiased wait-time aggregation.
- Support a task-scoped flamegraph that combines on-CPU and async-idle stacks
  in time units.
- Keep the disabled and non-selected poll paths allocation-free.
- Preserve source compatibility for the existing `TaskDumpConfig` API.

## Non-goals

- A process-wide mixed flamegraph in the first version.
- Correcting for tasks that were not spawned through dial9 instrumentation.
- A strict maximum number of captures in every wall-clock second. The
  configured rate is a long-run expected rate.
- Combining scheduler-event samples with on-CPU samples. Mixed flamegraphs use
  `CpuProfile` samples only.
- Changing Tokio's task-dump capture mechanism.

## Public API

The preferred configuration is:

```rust
use dial9::{TaskDumpConfig, TokioAttachOptions};

let options = TokioAttachOptions::builder()
    .task_tracking_enabled(true)
    .task_dump_config(
        TaskDumpConfig::builder()
            .captures_per_second_per_worker(10)
            .build(),
    )
    .build();
```

`captures_per_second_per_worker` is a positive integer. `0` is rejected at
build time; callers disable task dumps by omitting `task_dump_config`.

The default is 10 captures/s/worker. `rng_seed` remains available for
deterministic tests.

### Compatibility

`TaskDumpConfig` is a published builder API. Keep the existing
`idle_threshold(Duration)` builder setter and `idle_threshold()` accessor as
deprecated aliases during migration.

Internally the configuration should store one capture interval. The new
captures-per-second setter converts frequency to that interval; the deprecated
idle-threshold setter writes the same value. There must not be two independent
production sampling controls.

This preserves source compatibility while deliberately changing the behavior
from "mean cumulative idle time between emitted dumps" to "mean wall-clock
capture budget per worker." Release notes must call out that semantic change.

## Cost Model

For:

- `W`: runtime workers,
- `r`: configured captures/s/worker,
- `c`: seconds per capture,

the expected cost is:

```text
captures/s = W * r
added CPU cores = W * r * c
runtime capacity fraction = r * c
```

The capacity fraction is independent of worker count. Using a conservative
10 us capture cost and the proposed 10 Hz default:

```text
runtime capacity fraction = 10/s * 10 us = 0.0001 = 0.01%
```

An eight-worker runtime takes approximately 80 captures/s and consumes 0.0008
CPU cores. This is the intended operational meaning of the API.

### Measurement basis

A standalone release-mode benchmark used Tokio's real `trace_with` API and
dial9's capture-plus-trim path. A synthetic future with 24 nested async levels
produced 27 trimmed frames, representative of the observed workload depth.
Four 20,000-capture runs measured a 7.65 us median capture-plus-trim cost.

The model uses 10 us per selected capture to leave room for event encoding,
stack interning, and host or workload variation.

### Representative workload

An anonymized eight-worker workload contained eight instrumented tasks,
11,727 polls/s, and 1.869 active CPU cores. Applying the 10 us planning cost
gives:

| Rate/worker | Total captures/s | Active CPU penalty | Runtime capacity |
|---:|---:|---:|---:|
| 1/s | 8 | 0.0043% | 0.001% |
| 5/s | 40 | 0.0214% | 0.005% |
| **10/s** | **80** | **0.0428%** | **0.010%** |
| 20/s | 160 | 0.0856% | 0.020% |

The separate wake-tracing benchmark measured a 94.2 ns median added cost per
wake/poll. Conservatively assuming one wake per observed poll gives 0.00111
added CPU cores, 0.059% of the workload's active CPU, or 0.0138% of the
eight-worker runtime's capacity.

At the recommended 10 captures/s/worker, normal wake tracing plus task-dump
capture is therefore expected to add approximately 0.102% relative to observed
active CPU, or 0.0238% of runtime capacity. These are planning estimates from
one representative workload, not universal bounds.

The sampling decision still runs on each eligible pending transition. Its fast
path should be a worker-local counter update and branch; stack capture,
trimming, interning, and event encoding only run for selected transitions.

## Sampling Design

### Population

An eligible item is an instrumented task poll that:

1. is running on a worker whose attached runtime has task dumps enabled;
2. returns `Poll::Pending`;
3. is not the immediate synthetic re-poll caused by the previous task-dump
   capture.

Sampling is worker-local. Tasks may migrate between workers; the probability
stored on an event is the probability used by the worker that made that
capture.

### Coverage across tasks and workers

The capture budget is shared by all eligible transitions observed by one
worker; it is not assigned to a fixed subset of tasks. `TaskDumped<F>` wraps
every future created through dial9's instrumented spawn path, and each pending
transition consults the sampler on the worker currently polling it. Therefore:

- every attached runtime worker has its own sampler;
- every eligible transition from every dial9-instrumented task has a nonzero
  inclusion probability after calibration;
- task migration is naturally handled by consulting the destination worker's
  sampler; and
- a low-volume worker may reach `p_w = 1` and capture every eligible
  transition, while a busy worker shares its configured budget across all of
  its eligible transitions.

This supports sampling a fixed set such as eight instrumented tasks across all
runtime workers without task-specific configuration. "All tasks" still means
all tasks wrapped through dial9 instrumentation. Tokio tasks spawned outside
that path cannot be retroactively wrapped by runtime hooks and have zero
task-dump inclusion probability.

### Rate calibration

Each worker maintains a count of eligible pending transitions over a fixed
epoch. One second is a reasonable initial epoch.

For epoch `e + 1`, calculate:

```text
lambda_w = eligible transitions observed in epoch e / epoch duration
p_w = min(1, target captures per second / lambda_w)
```

Every eligible transition in the next epoch is selected independently with
probability `p_w`.

The probability is based only on observations that occurred before the
selection. It is therefore independent of the future idle duration and can be
used for inverse-probability weighting.

The first epoch is calibration-only. Mixed-flamegraph queries must clip away
that warm-up interval. This avoids an arbitrary initial poll-rate estimate and
prevents an attach-time capture burst.

### Efficient Bernoulli selection

When `p_w` is constant for an epoch, use a geometric skip counter instead of a
fresh random draw on every poll:

```text
skip = number of failures before the next Bernoulli(p_w) success
```

Each eligible transition decrements `skip`. At zero, capture and draw the next
skip. Reset the skip counter when an epoch installs a new probability.

The worker-local state contains:

```text
epoch start
eligible count
current inclusion probability
geometric skip counter
PRNG
sampling-active timestamp
```

Use the existing `SplitMix64` PRNG and derive independent worker seeds from
`rng_seed` plus worker identity. No process-global RNG or atomic increment is
needed on the poll path.

### Bursts and overload

Bernoulli sampling bounds expected cost, not the exact count in every second.
An emergency burst cap may protect against a stale rate estimate after a sharp
poll-rate increase, but hitting it makes that worker/epoch statistically
incomplete.

If a cap is implemented:

- record a dropped-capture count;
- mark the affected coverage interval;
- do not silently present that interval as an unbiased mixed profile.

The normal controller should target comfortably below the emergency cap so
this is an overload signal, not routine flow control.

## Capture and Emission

`TracedFuture` constructs the existing wrapper stack for every instrumented
task:

```rust
WakeTraced<TaskDumped<F>>
```

`WakeTraced` continues to record normal wake tracing for every instrumented
task. `TaskDumped` owns only the task-dump capture decision and reusable frame
buffer. The sampler itself is worker-local, so all `TaskDumped` futures polled
on one worker contribute to and draw from the same budget.

The intended poll flow is:

```rust
fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<F::Output> {
    if task_dumps_disabled_or_recorder_paused() {
        return self.inner.poll(cx);
    }

    let result = self.inner.poll(cx);
    let Poll::Pending = result else {
        self.frames.clear();
        return result;
    };

    // trace_with re-polls using the real waker. Do not treat the resulting
    // synthetic wake as another sampling opportunity.
    if self.just_captured {
        self.just_captured = false;
        return Poll::Pending;
    }

    // observe_pending updates calibration on every eligible transition.
    // A selection returns the exact probability used for this transition.
    let Some(inclusion_probability) =
        WORKER_TASK_DUMP_SAMPLER.with(|s| s.observe_pending(monotonic_now()))
    else {
        return Poll::Pending;
    };

    match self.frames.capture(self.inner.as_mut(), cx) {
        Poll::Ready(output) => {
            // The trace_with re-poll completed the future, so there is no
            // following idle interval to represent.
            self.frames.clear();
            Poll::Ready(output)
        }
        Poll::Pending => {
            self.just_captured = true;
            self.frames.emit_all_callchains(
                self.task_id,
                monotonic_now(),
                inclusion_probability,
            );
            Poll::Pending
        }
    }
}
```

The snippet is structural pseudocode; projection and recorder plumbing are
omitted. The ordering is normative:

1. poll the inner future normally;
2. when it returns `Pending`, suppress a capture-induced re-poll if needed;
3. update and consult the current worker's sampler;
4. only when selected, run `trace_with` and collect raw instruction pointers;
5. if the capture re-poll is still pending, trim and emit every callchain with
   the selected transition's probability; and
6. clear the reusable frame buffer.

Emission happens in the same selected path. The implementation no longer
retains a captured stack until a later poll and no longer applies an idle-time
emission decision.

The existing `just_captured` suppression remains necessary. The `trace_with`
re-poll can cause an immediate wake; that synthetic follow-up poll must not
consume a new sampling opportunity or start a capture loop.

One `trace_with` call can produce more than one callchain. All callchains from
the same capture share task ID, timestamp, and inclusion probability. Treat
them as one capture group keyed by `(task_id, timestamp_ns)`, not as independent
samples. The timestamp is read once and reused while emitting the group.

### Multi-callchain selection

Multiple callchains usually mean the task is waiting on several leaves in a
`select!`, `timeout`, graceful-shutdown wrapper, or nested future tree. The idle
interval belongs to the task once; it does not belong independently to every
leaf.

The checked-in demo trace validates that this is the normal case:

| Shape | Captures | Share of all captures |
|---|---:|---:|
| More than one callchain | 12,233 | 99.935% |
| I/O plus graceful-shutdown notification | 11,053 | 90.295% |
| I/O, shutdown, request operation, and timer | 624 | 5.098% |
| I/O, shutdown, and application semaphore | 524 | 4.281% |

The largest group is an active connection wait paired with a persistent
graceful-shutdown `Notify`. Equal weighting would incorrectly attribute about
half of normal connection idle time to shutdown handling. Do not divide a
capture's weight equally across its callchains.

Choose one representative stack for the time-weighted flamegraph:

1. Normalize and deduplicate the callchains, then find their longest common
   root-side suffix.
2. Mark a branch as control flow only from its surrounding stack structure:
   - a deadline `Sleep` is secondary when it is under a timeout combinator and
     a sibling represents the operation guarded by that same timeout;
   - a cancellation or graceful-shutdown wait is secondary when its stack
     traverses the cancellation/shutdown wrapper and another work branch
     exists.
3. Never demote a branch solely because its leaf is `Sleep` or `Notify`; either
   can be the task's primary work.
4. If one non-control branch remains, use it.
5. Otherwise, prefer a unique branch whose pre-common-suffix portion contains
   more application-owned frames than its siblings. Application ownership
   comes from symbol/source provenance, excluding the Rust sysroot, dependency
   registry, Tokio, and dial9 capture plumbing.
6. If no unique winner remains, represent the set with one synthetic
   `[awaiting any of N]` stack rooted at the common suffix. Include stable,
   sorted leaf labels in the synthetic frame and retain every raw callchain for
   the inspector.

This deliberately handles `timeout(operation, deadline)` differently from a
genuine `select!` over peer operations. The former normally selects the
operation stack; the latter remains an explicit ambiguous wait set unless one
branch is provably control flow or uniquely application-specific.

Selection must be deterministic and independent of callback order because
Tokio may change or randomize branch poll order. Whether a capture resolves to
a primary stack or a synthetic wait set, it contributes its idle weight exactly
once.

## Trace Contract

Add one field to `TaskDumpEvent`:

```rust
struct TaskDumpEvent {
    timestamp_ns: u64,
    task_id: TaskId,
    callchain: InternedStackFrames,
    inclusion_probability: f64,
}
```

Adding the field is wire-compatible because trace schemas are self-describing.
The JS decoder must treat it as optional so old traces continue to load.

`TokioRuntimesSource::segment_metadata` must emit the task-dump sampling
configuration as source-owned segment metadata:

```text
task_dump.captures_per_second_per_worker = "10"
task_dump.sampler = "per_worker_bernoulli_v1"
task_dump.worker.<worker_id>.sampling_active_ns = "<monotonic timestamp>"
```

These are entries in the `SegmentMetadataEvent` written into each trace
segment. They are not fields on `TaskDumpEvent`, recorder-level user metadata,
or metadata supplied by the application. The Tokio source owns them because it
owns the attached-runtime configuration and worker set. As with existing
runtime-to-worker metadata, the source adds them to the writer's merged
metadata cache, which carries them across segment rotation.

If one recorder has attached runtimes with different capture rates, the Tokio
source must emit
`task_dump.worker.<worker_id>.captures_per_second = "<rate>"` instead of the
scalar rate shown above. A worker publishes `sampling_active_ns` once, when its
calibration epoch ends. That one-time update is stored in the shared runtime
context for the Tokio source to collect; it does not add shared-state access to
the steady-state poll path. The `inclusion_probability` on each event remains
the authoritative value for statistical weighting.

The CPU profiling source already emits these separate segment-metadata
entries:

```text
cpu.profile.frequency_hz
cpu.profile.backend
cpu.profile.event_source
```

No new `CpuSampleEvent` field is required for the initial mixed flamegraph.

## Statistical Contract

For eligible pending transition `j`:

- `I_j` is 1 when selected and 0 otherwise;
- `p_j` is the recorded inclusion probability;
- `d_j` is the task's idle duration represented by that capture;
- `stack_j` is the representative primary or synthetic async stack selected
  from that capture's callchain group.

The Horvitz-Thompson contribution is:

```text
wait weight_j = I_j * d_j / p_j
```

For any stack group `G`:

```text
estimated wait time(G) = sum(wait weight_j where stack_j belongs to G)
```

Because the decision is made before `d_j` is known:

```text
E[I_j * d_j / p_j] = d_j
```

The estimator remains unbiased when probabilities differ by worker or epoch,
as long as every sampled event records the probability actually used.

Do not use raw task-dump counts as time weights. Faster-polling tasks produce
more eligible transitions, and changing traffic changes `p_j`.

## Task-Scoped Mixed Flamegraph

### Why task scope is required

Task dumps only exist for tasks spawned through dial9's instrumented APIs on a
runtime with task dumps enabled. An uninstrumented task has inclusion
probability zero.

A runtime-wide graph that combines:

- CPU samples from every task, and
- async stacks from only instrumented tasks

systematically overstates on-CPU time for uninstrumented tasks and understates
their idle time. Inverse-probability weighting cannot recover a population
with zero-probability members.

The first mixed-flamegraph UI must therefore be reachable from a single task's
detail view and include only:

- CPU samples attributed to polls of that task;
- task dumps whose `task_id` is that task.

If the task has no dumps in the selected window, show insufficient data rather
than a CPU-only graph labeled mixed. A future multi-task view requires explicit
task-dump eligibility metadata and must reject partially eligible selections.

Clip the selected task's CPU and idle inputs to the sampling-active timestamps
published for workers that polled the task. If the task migrated, use the
latest activation timestamp among those workers as the effective range start.
Do not offer a mixed view when the required activation metadata is missing.

### CPU weights

Read the sampling frequency from segment metadata:

```text
frequency_hz = Number(segmentMetadata["cpu.profile.frequency_hz"])
cpu weight = 1_000_000_000 / frequency_hz
```

Use only `CpuProfile` samples attached to the selected task's poll intervals.
Each sample contributes the same expected nanoseconds at the configured
frequency.

If the metadata is absent or invalid, the viewer cannot put CPU samples and
task dumps in common time units. Keep the existing count-based CPU flamegraph,
but do not offer the mixed view.

### Task-dump weights

For each selected capture group:

1. identify the idle interval beginning at its capture timestamp;
2. end the interval at the next poll start for that task;
3. intersect the interval with the selected time window;
4. select its representative stack using the multi-callchain rules above; and
5. divide the overlap duration by `inclusion_probability`.

When wake data can reliably distinguish the external wake from capture-induced
wakes, split the interval into:

- `[async-wait]`: poll end to external wake;
- `[runnable]`: external wake to next poll start.

Until then, use one `[idle-at-await]` category for poll-end to next-poll and
state that it includes scheduler delay.

The representative primary or synthetic stack receives the full
inverse-probability weight once:

```text
capture weight = idle overlap / inclusion_probability
```

### Tree construction

Prefix the two stack domains with synthetic frames:

```text
[on-cpu]
[idle-at-await]
```

This prevents equal symbol names in physical CPU stacks and logical async
stacks from merging accidentally.

`buildFlamegraphTree` already accepts a numeric `weight` on each sample. Pass
the computed nanoseconds directly. Do not expand a weighted sample into
repeated objects.

The root total is estimated task time represented by the two sampled domains.
Synchronous off-CPU time inside a poll is not represented by either source and
remains a documented limitation.

## Rejected Alternatives

### Sample only at emission

This is the current behavior. It limits trace bytes but still takes a stack
dump on every pending poll, so it does not bound capture overhead.

### Wall-clock timer, capture the next pending transition

This controls the rough rate but gives transitions unequal inclusion
probabilities based on the preceding inter-poll gap. The resulting
inverse-probability estimator has unnecessarily high variance.

Calibrated Bernoulli sampling keeps probabilities approximately uniform within
each worker epoch while still targeting captures per second.

### Fixed fraction of pending polls

A fixed fraction has simple statistics but no stable cost. Capture rate scales
directly with workload poll rate, which is the quantity the API is intended to
decouple from overhead.

### Per-task capture budget

A per-task rate makes total cost scale with task count and allows many
short-lived tasks to exceed the intended runtime budget. Worker-local budgets
match where capture CPU is spent and keep runtime-capacity cost stable.

### Process-wide mixed flamegraph

CPU profiling covers more tasks and threads than task dumps. Joining those
populations without explicit eligibility produces structural bias, not merely
sampling noise. Task scope is the correct first interface.

## Implementation Plan

1. Replace `TaskDumpConfig.idle_threshold` internally with one worker capture
   interval and add the new builder setter plus deprecated aliases.
2. Replace the thread-local task-dump config cell with worker-local sampler
   state initialized by runtime thread hooks.
3. Move the sampling decision to the `Poll::Pending` path before
   `FrameBuf::capture`.
4. Emit selected captures immediately and remove delayed idle-time emission
   state.
5. Add `TaskDumpEvent.inclusion_probability` and decode it as optional in JS.
6. Have `TokioRuntimesSource` emit task-dump configuration and per-worker
   sampling-active segment metadata.
7. Group sibling callchains and implement deterministic representative-stack
   selection with an explicit ambiguous-wait fallback.
8. Extend the
   [telemetry integration test application](telemetry-integration-test-app.md)
   and both conformance adapters with the sampled task-dump contract. This is a
   prerequisite for mixed-flamegraph implementation.
9. Add task-scoped mixed-flamegraph sample construction using CPU frequency
   metadata and inverse-probability task-dump weights.
10. Keep old traces on their current unweighted task-dump rendering path.

## Telemetry Integration Test Prerequisite

The reusable
[telemetry integration test application](telemetry-integration-test-app.md)
owns the runnable workload, trace-contained contract, local/aggregate
conformance adapters, and CI workflow. It covers CPU profiling, spans, and task
dumps rather than being specific to this feature.

The app and both adapters must land before task-dump mixed flamegraphs. This
design extends its task-dump scenario with inclusion probabilities,
representative-stack categories, and inverse-probability wait estimates. No
sidecar manifest or task-dump-specific test application should be introduced.

## Test Plan

### Focused tests

- Builder default, nonzero validation, deprecated alias, and deterministic
  seed tests.
- Sampler tests showing the long-run per-worker rate converges to the configured
  target over different pending-poll rates.
- Statistical tests showing each selected event records the probability used
  and inverse-probability totals converge to known synthetic wait totals.
- Regression test proving non-selected polls do not call `trace_with`.
- Regression test proving every selected capture whose re-poll remains pending
  emits and no second emission sampler remains.
- Existing no-extra-wake/no-extra-poll and completed-on-repoll tests.
- Trace round-trip tests for the new optional event field.
- JS parser test for old events where `inclusion_probability` is undefined.
- Viewer tests that mixed profiles:
  - use `cpu.profile.frequency_hz`;
  - include only CPU samples and dumps for the selected task;
  - reject missing CPU metadata or missing task dumps;
  - group sibling callchains by task ID and capture timestamp;
  - select timeout operations over their paired deadline timers;
  - select work over recognized cancellation/shutdown branches;
  - preserve standalone timer and notification waits;
  - produce an order-independent synthetic wait set for ambiguous peer
    branches;
  - apply one full inverse-probability weight per capture group;
  - clip idle duration to the selected window;
  - pass numeric weights directly to the flamegraph builder.

### Integration conformance

Extend the telemetry integration fixture and run its trace through both the
local JavaScript and aggregate Rust production paths. In addition to the
shared contract checks defined by that design, assert:

- both instrumented tasks produce task dumps and retain their distinct stack
  distributions;
- every worker that observes eligible transitions participates in calibration
  and sampling;
- the observed capture count is consistent with
  `sum(min(worker_pending_rate, configured_worker_rate))`; for the saturated
  test workload this reduces to
  `workers * captures_per_second_per_worker`;
- inverse-probability weighted async time and frequency-weighted CPU time fall
  within declared statistical tolerances of each task's by-construction
  percentages;
- the lock-wait and timer-await symbols remain distinct;
- the timeout, cancellation, peer-select, standalone-timer, and
  standalone-notification scenarios resolve to their specified representative
  stacks;
- the uninstrumented CPU and off-CPU threads do not appear in either
  task-scoped mixed graph; and
- changing the capture rate changes sample count and variance, but not the
  estimated percentages outside the declared tolerance.

Keep this as an end-to-end statistical test with explicit confidence bounds,
not exact sample-count assertions. The dedicated canonical telemetry fixture
is separate from the viewer demo trace.
