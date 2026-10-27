# Startup sampling protocol checks

`StartupSampling.tla` is a bounded abstraction of the implemented slot protocol,
not a refinement proof of the Rust implementation. It models three processes,
one slot, finite nonce versions, delayed conditional writes/acknowledgements,
frozen timeout decisions, renewal, lease expiry and process exit. Processes do
not restart in this model. Membership estimation, caller target changes, Tokio
runtime wiring and real-time scheduler delays are tested or documented separately.

The adversarial review adds [population bounds and limitations](estimation.md),
a [storage lifecycle decision](storage-lifecycle.md), and the October 26
[remediation and coverage results](review-results.md). Those qualifications
are essential: the bounded model does not prove estimator convergence, policy
retention, or containment of a failed maintenance task.

Run TLC with Java 17 and a current `tla2tools.jar`:

```sh
java -XX:+UseParallelGC -cp /path/to/tla2tools.jar tlc2.TLC \
  -config continuity.cfg StartupSampling.tla
java -XX:+UseParallelGC -cp /path/to/tla2tools.jar tlc2.TLC \
  -config strict.cfg StartupSampling.tla
java -XX:+UseParallelGC -cp /path/to/tla2tools.jar tlc2.TLC \
  -config continuity-cap-counterexample.cfg StartupSampling.tla
```

On October 23, 2026, continuity explored 185,744 distinct states and strict mode
189,008, with no invariant violations. Both check frozen negative decisions,
terminal automatic stops and the coordinated slot cap. Strict additionally checks
actual profilers, assuming a local stop is enforced before server reuse and callers
do not manually resume recording. Manual recorder controls are outside the model.
These assumptions exclude arbitrary process suspension.

The model abstracts away timestamp precision and maintenance-task failure.
October 26 regressions and fixes cover both: native S3 declares one second of
modification-time uncertainty, which delays reuse even with zero clock skew;
a task-drop guard invalidates coordination and stops strict mode on maintenance
failure. A closed strict-mode monitor also completes the caller-owned stop future. The sampler never
resumes it automatically; callers can explicitly resume a runtime whose Tokio
hooks were installed initially. These tests supplement the model; its bounded invariants are not a
refinement proof or a scheduler guarantee.

Policy refresh and S3 lifecycle are outside the TLA+ model. Paused-time Rust
tests run 28 simulated days with fourteen-day age-based expiration, check that
live coordination remains fresh and retired records disappear, and exercise
policy-write failure and conflict under both lease-loss choices. Their collector
rechecks each current object's age at deletion; it is a simulation assumption,
not an observation of S3's internal queue or a fourteen-day production soak.

The continuity-cap-counterexample intentionally fails `ActualCap` after 936
states: an original profiler keeps recording after expiry, a replacement claims
the same slot, and both record. That behavior is permitted by the default
continuity policy. It must remain an expected counterexample rather than be
reported as a failure of the implementation or hidden as a hard-cap guarantee.

## Workload simulation

Qualification tests exercise 100,000-process cold starts in one large pool,
1,000 evenly sized pools and 1,000 uneven pools. A separate finite-capacity
backend tests overloaded simultaneous startup. These simulations exercise the
production protocol, not production throughput. See [estimation.md](estimation.md)
for assumptions, bounds and commands.
