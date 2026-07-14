# Schema-Driven Viewer Specs Implementation Checklist

Status: active frontend implementation checklist for `574-generalize-series-and-fields` / PR 667.

Design contract: [`schema-driven-view-specs.md`](schema-driven-view-specs.md).

Update this file as work lands. Check an item only after its implementation and focused tests are complete. Keep deferred Rust/wire work separate from the frontend PR.

## Preparation And Documentation

- [x] Document explicit point and half-open interval semantics.
- [x] Document counter decrease, missing-sample, and rebase behavior.
- [x] Correct CPU examples to apply `rate` before addition.
- [x] Document `hold_until_next`, stable final bounds, mark compatibility, clipping, and downsampling invariants.
- [x] Document component/recording identity requirements and conservative multi-trace behavior.
- [x] Document continuous value coloring as deferred work.
- [x] Record the proposed Rust/wire direction without adding a public API.
- [ ] Rebase the branch onto `upstream/main` while preserving the four existing feature commits.
- [ ] Run focused baseline tests after the rebase and record any pre-existing failure before implementation.

## Expression Evaluation And Temporal Support

- [ ] Represent each evaluated series result internally as a value plus point/interval support.
- [ ] Evaluate stateful operations at their AST node rather than only at the top-level expression.
- [ ] Make `rate(value, time)` produce `[previous_time, current_time)` support.
- [ ] Preserve interval support through arithmetic with constants and metadata.
- [ ] Require identical support when arithmetic combines two interval values.
- [ ] Preserve support through scaled, summed, and otherwise nested rates.
- [ ] Keep previous-sample state independent per component/recording and `partition_by` key.
- [ ] Skip a decreasing-counter interval and rebase from the current valid sample.
- [ ] Ensure missing, non-numeric, and non-finite samples do not replace the last valid baseline.
- [ ] Reject non-positive rate windows without producing non-finite output.
- [ ] Add focused evaluator tests for nested rates, incompatible support, resets, missing samples, and partitions.

## Snapshot-To-Interval Transform

- [ ] Implement series transform `{ op: "hold_until_next" }`.
- [ ] Emit `[current_time, next_time)` for each snapshot gauge.
- [ ] Bound the final snapshot by `trace.recordMaxTs`, never by the viewport.
- [ ] Omit a final zero/negative-duration interval.
- [ ] Apply the transform before summaries, downsampling, hit testing, and rendering.
- [ ] Use invalid snapshot timestamps to end the previous interval and preserve the resulting gap.
- [ ] Migrate Socket Accept Queue snapshot gauges to `hold_until_next`.
- [ ] Add tests for final bounds, exact shared boundaries, and viewport-independent output.

## Range And Rendering Semantics

- [ ] Add shared pure helpers for half-open point inclusion and positive interval overlap.
- [ ] Use the shared range rules in summaries, Y-domain calculation, hit testing, and drawing.
- [ ] Weight interval means by visible overlap duration.
- [ ] Make `line` consume points and interpolate only the visible crossing from adjacent points.
- [ ] Keep fully offscreen line values out of the visible Y domain.
- [ ] Make `step_line` consume intervals and draw vertical transitions only between contiguous intervals.
- [ ] Make `step_area` consume intervals without filling gaps.
- [ ] Clip lines, step marks, thresholds, and guides to the chart rectangle.
- [ ] Resolve hover at a shared boundary to the interval starting at that boundary.
- [ ] Include every resolved series/partition in the legend.
- [ ] Preserve gaps, interval boundaries, partition identity, and tooltip provenance during downsampling.
- [ ] Keep summaries independent from downsampled geometry.
- [ ] Add focused tests for clipping, boundary overlap, connectors, gaps, line domains, hover, legends, and downsampling.

## CPU Migration And Multi-Trace Safety

- [ ] Change CPU to `rate(user_cpu_ns) + rate(system_cpu_ns)` in the generic spec.
- [ ] Verify that a reset in either CPU component omits the affected interval even when the other component increases.
- [ ] Preserve weighted average, maximum, capacity guide, tooltip values, folding, and custom-event visibility.
- [ ] Keep CPU values unclamped above available parallelism.
- [ ] Restore `display.y_max: 1` as an expandable initial-domain hint.
- [ ] Add the conservative guard that skips stateful series when multiple components are known but unique stream identity is unavailable.
- [ ] Emit one useful diagnostic for an unsafe multi-component series rather than one message per event.
- [ ] Remove the event-specific CPU DOM, state, renderer, listeners, analysis helper, and obsolete tests after generic-path parity is covered.

## Frontend Verification

- [ ] Run `node dial9-viewer/ui/test_trace_analysis.js`.
- [ ] Run `node dial9-viewer/ui/test_panel_layout.js`.
- [ ] Run any additional touched `dial9-viewer/ui/test_*.js` files.
- [ ] Register any newly created JS test file in `scripts/e2e-trace-tests.sh`.
- [ ] Run `cargo build -p dial9-viewer` to verify embedded UI assets.
- [ ] Review the final diff for stale CPU-specific code and accidental scope expansion.

## Acceptance Criteria

- [ ] A reset of one CPU component creates one gap and the next valid interval recovers from the new baseline.
- [ ] A scaled or summed rate retains `[previous_time, current_time)`.
- [ ] An interval ending exactly at `viewStart` is neither rendered nor included in the domain or summaries.
- [ ] A snapshot gauge held across the visible range contributes the correct duration to aggregates.
- [ ] An invalid snapshot ends the previous held interval and leaves a gap until the next valid snapshot.
- [ ] A `step_line` has vertical connectors only across contiguous intervals.
- [ ] Downsampling never bridges a semantic gap.
- [ ] No series, guide, or threshold geometry escapes the chart rectangle.
- [ ] Multi-component counters without proven stream identity produce a diagnostic instead of a false rate.
- [ ] Panning and zooming do not change the semantic end time of the final interval.

## Deferred Rust And Wire Work

- [ ] Add field-local `metric.kind` semantics alongside `unit` in `FieldAnnotation`.
- [ ] Define a versioned bundle for `computed_fields` and `views` in trace-segment metadata.
- [ ] Preserve recording/component identity and metadata scope through parsing and file combination.
- [ ] Validate trace-provided JSON AST without executing JavaScript.
- [ ] Add limits for bundle size, expression depth, series count, and partition cardinality.
- [ ] Decide compatibility behavior for unknown bundle versions.
- [ ] Add Rust builders or macros only after the JSON shape is validated by the frontend implementation.
- [ ] Reconsider dedicated schema-level metadata only if event-name association is insufficient.

## Explicitly Deferred Viewer Features

- [ ] Design continuous value-driven color scales separately from discrete thresholds.
- [ ] Unify built-in and custom event sources without duplicating all events in memory.
- [ ] Define general multi-source joins and pairing.
- [ ] Add generic `interval_bars`, `markers`, `heatmap`, and `flamegraph` renderers.
