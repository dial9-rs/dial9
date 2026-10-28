# Sampling a changing fleet

The uploadable artifact is `sampling-fleet-laboratory.html`. It embeds all styles,
simulation code, chart rendering, and controls. It makes no network requests and
has no runtime dependencies.

This resource belongs outside the implementation PR. It models the October 28
change from rank-based admission/stopping to independent admission and fresh
Bernoulli thinning with time-varying expansion weights.

## Reproduce

```sh
python3 build.py
node check.js
```

`check-browser.js` additionally exercises all four scenarios, the fallback
control, time cursor, sparse distributions, and absence of horizontal overflow
at 320, 360, 680, 1000 and 1500 pixels. It uses the existing local Playwright
installation and a local Chromium binary; set `CHROMIUM_EXECUTABLE` if needed.
It aborts HTTP/HTTPS requests and takes no screenshots.

`reference-results.json` records the four default seeded fleet outcomes. These
are examples, not throughput or latency measurements.

## What the simulation preserves

- One tracked slice from an independent uniform process-identity rank.
- One frozen startup choice from an independent admission draw, conditional on
  tracking, with weight `1/(f*r)`.
- Explicit Disabled or AssumePopulation startup fallback.
- Membership of unarmed, admitted, and terminal carrier processes in current
  and previous five-minute buckets.
- Minute polls with fixed per-process jitter, randomized heartbeat offsets,
  guarded census windows, and four-count plateau settlement.
- One abstract fenced census leader, with delayed takeover of a departed leader.
- Sealed fresh thinning rounds when admitted counts exceed the soft trigger.
  Each observed round is applied at most once. Successful survivors multiply
  their weight by `1/q`; unsuccessful processes stop permanently.
- A 190-second thinning-round cooldown for the default poll/skew settings while
  above the trigger. A new fresh census at/below the trigger clears the round.
  New joiners skip rounds present when they start.
- Optional graceful exits, failed cleanup/crash ghosts, short lifetimes,
  overlapping rolling processes, and storage outages.
- Immutable census observations and latest-preceding, maximum-age queries.
- Explicit zero-support member markers, retained as a subset of unarmed counts.
- Per-unit chronological expansion-weight examples, including terminal zero.
- Ground-truth process population, auxiliary physical-machine counts, actual
  recording units, census `R/f`, and the sum of surviving expansion weights.

## Statistical limits

Census Gaussian curves describe random hash error under complete, live,
independent membership. They exclude stale data, ghosts, missing writes, and
changes since the observation. Full tracking has zero hash error.

Independent next-cohort admissions are simulated with exact binomial draws.
Sparse-census delta-method Gaussian curves are suppressed when expected tracked
count is below `32*target`; population Gaussians are suppressed below 30 expected
tracked members. The weighted-estimate Gaussian is fitted to empirical moments
and is descriptive, not a confidence guarantee. Low count discreteness and skew
remain visible in the histograms.

An unarmed zero-probability cohort has no recoverable fleet trace coverage.
The sum of trace weights estimates the supported population in expectation,
assuming workload/lifetimes are independent of fresh coins and trace data is
fully delivered. Population history alone does not supply trace inclusion
weights. The simulation treats operations and trace delivery as instantaneous;
it does not implement full concurrent storage CAS schedules or partial LIST
failures.

Production metadata is now one evaluated scalar snapshot, as described in the
artifact's weight examples. The simulator evaluates instantaneous factors at
the cursor; it does not model additional error from applying one captured
weight to a whole trace segment.
