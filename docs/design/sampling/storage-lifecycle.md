# Roster storage, lifecycle and rollout

The S3 adapter defaults to `_dial9_sampling/`. The v2 namespace separates
persistent roster objects from disposable bucketed membership:

```text
_dial9_sampling/v2/pools/<SHA256-canonical-pool>/roster.json
_dial9_sampling/v2/members/<SHA256-canonical-pool>/<bucket>/
    <rank16>.<u|a|c>.<fresh-incarnation>
```

The pool hash is SHA256 of JSON `(scope, sorted dimensions)`. Rank adds host
identity to that canonical tuple. Duplicate dimension names and blank identities
are rejected. JSON encoding distinguishes embedded delimiters and newlines.
A stable identity preserves rank; every invocation still gets a fresh ULID.
The roster policy includes target, slice, overshoot and timing parameters.

## Operations and permissions

Tracked startup needs one GET. Active/unarmed/carrier maintenance needs GET,
conditional roster PUT, unconditional own-member PUT and own-member DELETE.
Only elected leaders LIST two member prefixes, with all pagination completed.
Member bodies are empty and never fetched. Directory-bucket ordering is not
assumed: collection validates and sorts/deduplicates locally.

Conditional roster writes use one SDK attempt. Lost write acknowledgements
remain unknown outcomes for GET reconciliation; SDK retries must not turn a
committed write into a misleading conditional failure. The roster is never
unconditionally deleted. Delete permissions can be scoped to the member prefix.
Within an incarnation, carrier state takes precedence over active/unarmed
before bucket recency, including delayed PUTs after clock adjustment.
The code only deletes its own incarnation; IAM alone does not establish that
per-process ownership when consumers share credentials.

A known incompatible startup roster applies fallback and creates no maintenance
session. Unknown/errored startup reads must confirm absence or compatibility in
background before registering members. If a later read shows an incompatible
policy, the worker withdraws its own members and stops PUTs, preserving its
frozen recording choice. It must never rewrite another policy's roster.

## Retention

Use a days-based lifecycle rule for the member prefix only. One day is ample
for default five-minute buckets; custom retention must comfortably exceed two
bucket widths plus clock/request margins. Live processes heartbeat once per
bucket at a fixed incarnation-specific offset and retry failed writes during
later maintenance. Current and previous buckets bound census visibility of a
crashed process to roughly two bucket widths plus bounded skew. Physical old
objects can remain until asynchronous lifecycle expiration.

Do not expire the roster. Startup intentionally may read a prior settled arm
from an idle pool; expiration would turn a continuing pool back into bootstrap.
Roster persistence is also part of the version-fencing argument. Storage
retention does not enforce leases, publish cutoffs or establish statistical
freshness. If versioning is enabled, consider noncurrent versions and delete
markers separately.

Shutdown attempts bounded deletes of the invocation's current/previous member
keys. A trimmed process can leave both active and carrier keys when cleanup
fails, so shutdown can have up to four keys to delete. A cancelled member PUT
may still commit server-side; incarnation isolation makes that safe for the
replacement, while bucket visibility/lifecycle bounds stale retention.

## Policy rollout and bootstrap

All processes in one full scope/dimension pool must agree on policy. Changing
policy in place is not an automatic rollout: the persistent roster rejects
incompatible metadata. Use a coordinated drain/reset or a deliberate new scope.
A new namespace can overlap recording with an old one; size that migration
explicitly rather than assume a shared quota.

Cold `Disabled` is deliberately terminal for that invocation, although a census
can settle for subsequent startups. Use `AssumePopulation(n)` when bootstrap
coverage is needed, with an explicit operational population assumption. It is
not a bound on actual volume. A fixed sparse slice additionally requires a
credible lower population bound per complete dimension tuple.

## Primary storage references

- [S3 consistency and per-key updates](https://docs.aws.amazon.com/AmazonS3/latest/userguide/Welcome.html)
- [ListObjectsV2, including directory-bucket order](https://docs.aws.amazon.com/AmazonS3/latest/API/API_ListObjectsV2.html)
- [Age-based lifecycle and overlapping filters](https://docs.aws.amazon.com/AmazonS3/latest/userguide/lifecycle-configuration-examples.html)
- [Expiration and versioning considerations](https://docs.aws.amazon.com/AmazonS3/latest/userguide/lifecycle-expire-general-considerations.html)
