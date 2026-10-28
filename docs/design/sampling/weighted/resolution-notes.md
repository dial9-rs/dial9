# Review resolutions

The reviewed implementation is pinned in `validation.json`; original reports
retain their findings and evidence rather than silently rewriting them.

- Lifecycle review: weak sampling stops are permanently terminal; ordinary
  pause/resume still works. Recorder ownership and normal shutdown remain with
  the application. Concurrent permanent-stop/resume checks passed Shuttle.
- Protocol review: backend timestamp uncertainty participates in the census
  guard and impossible windows are rejected before backend work. Complete
  observations survive membership cleanup and roster replacement.
- Statistics review: fresh admission entropy is separate from census rank.
  Sealed fresh Bernoulli rounds replace sorted-rank trimming. Survivors record
  inverse conditional factors once per applied epoch; terminal decisions record
  zero before the asynchronous stop notification. Missing/overflow evidence is
  unknown rather than a plausible factor.
- Weighted storage review: documentation now states probabilistic count
  behavior. Actual Worker regressions verify unchanged weights on duplicate
  epochs, multiplication on a fresh epoch, and startup skipping of an already
  published round. No exact-count invariant is claimed.
- Trace weights integration: callbacks stop metadata even for fallback sessions
  without a maintenance worker. Conflicting source registration fails closed.
  The real encode/decode test exercises 10 → 20 → 0 inside one segment.

The count archive is observational evidence and does not prove a roster CAS
succeeded. Full roster history is not needed by a decoder: traces already carry
the expansion factor and its local effective timestamp, updated by the existing
roster refresh. Retained census history answers the separate population question.
