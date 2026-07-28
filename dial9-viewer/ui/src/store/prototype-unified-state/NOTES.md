# Prototype notes

## Provisional answer

The model can represent the meaningful state categories found in all five page
audits without storing browser resource handles. The important property is one
transition authority, not one undifferentiated state object.

The prototype makes four design choices concrete:

1. Semantic intents can update several fields atomically; generic slice patches
   are unnecessary.
2. URL projection can be automatic after a committed durable transition while
   hydration remains write-free.
3. Request identity and stream sequence belong in the model; cancellation
   handles do not.
4. The frozen flamegraph can remain internally mutable only when its state is a
   proposal that is reconciled back to a canonical model projection.

## Still to decide before production

- Whether typed Views should be hand-written or derived from a field policy.
- Whether immutable parsed traces remain directly in the model or move behind
  a `TraceRef` resource registry.
- How strict the module-scope mutable-state architecture check should be before
  it becomes noisy.
- Whether flamegraph reconciliation is fast enough during gestures or needs a
  gesture-local proposal channel.

Delete this directory after the decisions are absorbed into the production
State Authority Module and its ADR.
