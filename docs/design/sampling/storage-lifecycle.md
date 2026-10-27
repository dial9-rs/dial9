# Sampling storage and lifecycle

Use separate trace and coordination prefixes in the same bucket, with matching
days-based retention when their operational requirements permit it:

```text
Traces:       s3://YOUR_BUCKET/dial9-traces/version=...
Coordination: s3://YOUR_BUCKET/_dial9_sampling/dial9-traces/v1/pools/<hash>/...
```

`S3SamplingStore::builder().prefix(...)` also supports explicit placement or a
separate bucket when ownership, IAM or retention differ. Producers need scoped
GET, conditional PUT and membership DELETE permissions, plus bucket LIST.

## Separate versus nested prefixes

A nested `dial9-traces/_dial9_sampling/` namespace could reuse a broad object
prefix grant and its parent retention rule. Upload-only roles would still need
GET/DELETE, and prefix-restricted LIST permissions may require changes. Separate
prefixes allow independent IAM and operational inspection, with two lifecycle
rules sharing a retention duration. Neither placement requires a member-only
cleanup rule. Inspect overlapping rules: a longer coordination rule cannot
override a shorter applicable parent or bucket-wide rule.

## Expiration and lease validity

Application leases last five minutes and renew every minute. Those deadlines
control slot reuse without deleting the S3 object. Policy is conditionally
refreshed before admission and lease extension when its age exceeds the renewal
interval. A failed or unconfirmed policy refresh cannot extend a lease. Active
slots and sampled members are also rewritten periodically. Replacement resets
last-modified time, keeping active records young while abandoned records age out.

Retention must comfortably exceed the lease duration plus refresh, request,
timestamp and clock-skew margins. Fourteen-day trace retention can also cover
coordination. Absolute-date expiration is unsuitable because replacement does
not move that deadline. Lifecycle is asynchronous and operates at day granularity;
it is neither a lease timer nor a fencing mechanism.

| Record | Cleanup |
| --- | --- |
| `policy.json` | Retired dimensions eventually expire; live policy refreshes before admission/renewal. |
| `slots/*.json` | Renewals keep held slots fresh. Owner-checked release writes conditional tombstones. |
| `members/<rank>/<process>` | Members heartbeat and delete on graceful exit. Freshness filters exclude crash leftovers before lifecycle removes them. |
| Noncurrent versions | Configure separate expiration if bucket versioning is enabled. |
| Delete markers | Configure expired-marker cleanup where applicable. |

Never unconditionally DELETE shared slots: a late claim or renewal can race that
delete. Application deadlines and conditional ownership checks govern reuse.

## Rollout and validation

Before enabling expiration, every participating owner must refresh live policy.
Adding a rule applies to existing records, including aged policies. Verify
policy refresh, all applicable lifecycle rules, versioning and prefix permissions
before rollout. Changing coordination namespace creates independent quotas;
drain old owners before moving to a new namespace.

Paused-time tests cover 28 simulated days with fourteen-day expiration, abandoned
records, aged policy refresh and refresh errors/conflicts in both lease-loss
modes. Their collector rechecks the current object's modification time at deletion.
This is an age-based simulation assumption; it does not observe S3's expiry queue
or establish a fourteen-day production soak.

## S3 references

- [Lifecycle configuration examples and overlapping filters](https://docs.aws.amazon.com/AmazonS3/latest/userguide/lifecycle-configuration-examples.html)
- [Age-based expiration and replacement timestamps](https://docs.aws.amazon.com/AmazonS3/latest/userguide/intro-lifecycle-rules.html)
- [Expiration and versioning behavior](https://docs.aws.amazon.com/AmazonS3/latest/userguide/lifecycle-expire-general-considerations.html)
