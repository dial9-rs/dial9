# Safety arguments and their assumptions

These arguments describe the protocol. Finite TLC checks are not a refinement
proof of the Rust program.

## Publication fencing

Assume per-object linearizable conditional PUT, reliable opaque versions, and
no deletion or external replacement of the roster. Every successful mutation
increments a checked epoch and changes its revision nonce. Even a body-derived
version cannot repeat a previous successful body's version. Overflow fails.

A process publishes against a version confirmed while that document named its
own incarnation. A takeover mutates the same document, changing its version
at the takeover's linearization point. A paused former leader's PUT against
its old version then fails. Checking a separate lease object would not supply
this atomicity.

A committed PUT with a lost response still changes the server version. The
writer discards local authority and does not blindly retry. A subsequent GET
can confirm its own fresh lease and restore authority, or observe a successor.
Neither permits publication against an unconfirmed version.

This proves fencing after successful takeover, not prompt failure detection.
A formerly confirmed leader can issue LISTs after a pause; its final CAS stays
fenced. Lease expiration uses server LastModified plus three poll intervals,
configured clock skew and backend timestamp uncertainty. JSON expiry is diagnostic.

## Frozen startup and terminal trimming

Startup chooses once from a compatible settled arm cutoff or the caller's
failure policy. Maintenance has no unarmed-to-active transition. Public
`enabled` retains that initial choice; live diagnostics and private stop signals
are separate. Only active-to-carrier is permitted. A carrier participates in
membership and election without becoming active again.

Registered recorders receive an idempotent terminal disable. Invocation cleanup
also disables them. These properties concern sampler-controlled changes;
application code explicitly resuming a recorder is outside the guarantee.

## Incarnation cleanup

Every invocation receives a fresh incarnation, including invocations sharing a
stable rank identity. Its membership PUTs and DELETEs contain that incarnation.
Deleting a prior invocation's keys cannot delete a replacement's keys. State
precedence applies within one incarnation, so an abandoned carrier cannot
classify the new process as carrier. Dedupe prioritizes carrier over active over
unarmed before comparing buckets. A delayed active PUT into a newer bucket
therefore cannot defeat a visible carrier key after a backward clock
adjustment. If carrier heartbeats fail and its key leaves both visible buckets,
the census has incomplete state evidence; this rule does not preserve terminal
history across missing membership. The process and recorder still stay terminal.

The census counts processes, including overlapping restarts. Stable identities
correlate those ranks. Statistical independence requires a unique rank identity
for each simultaneously live process.

## Census and bucket guards

Current and previous prefixes must finish all pages. Objects are validated and
deduplicated by incarnation; ordering is irrelevant. Invalid members, inconsistent
ranks, repeated continuation tokens, request errors or pass timeouts cannot
publish a replacement census.

An unsafe phase skips LIST and only renews leadership. A real LIST can update
counts, keep boundary and settling history only if it starts and finishes in
the same local bucket and in the safe phase:

```text
g <= time % bucket_width < bucket_width - g
g = 2 * max_clock_skew + backend_timestamp_uncertainty
```

Otherwise count timestamps remain old, keep is cleared and history resets.
Zero members never establishes a usable arm. A prior settled arm may be carried
for continuity. Takeover clears keep, so renewing a lease cannot freshen an old
trim boundary. Trim additionally requires a census timestamp, age at most
three polling intervals plus twice clock skew, and a timestamp no more than
twice clock skew into the local future. A delayed same-version CAS can refresh
server LastModified while its computed census is old; lease freshness alone
therefore cannot authorize trimming. Arbitrarily old arm cutoffs remain usable
for explicit startup continuity.

The skew assumption bounds each host relative to storage UTC. A backend's
uncertainty must leave a nonempty safe window. These guards address bounded
skew and rollover. They do not make LIST atomic
or prove every eligible process registered. Four nearly flat readings remain a
convergence heuristic: failed writers can make an incomplete census look stable.

## Conditional overshoot bound

Let `C = target + max_overshoot`. A complete pass sorts active `(rank, incarnation)`
pairs. The pair at zero-based index C is the first excluded boundary: exactly C
observed pairs are below it. Incarnation breaks equal-rank ties.

If every active process is represented, all read/apply the boundary, and no new
process starts during reconciliation, at most C remain active. New starts,
failed member writes, partitions, stalled leaders and manual recorder resumes
invalidate those premises. Healthy reconciliation requires a leader pass and a
follower poll plus request time; it can approach two poll intervals, with jitter.
There is no unconditional count cap or finite outage bound. Shrinkage can leave
too few profilers because terminal states cannot later be enabled.
