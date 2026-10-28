# Span Explorer latency investigation

The latency investigation view lives in **Span Explorer**, below the selected span
type's duration histogram and opens automatically when a span is selected,
including a deep link. It works from
the current source, exact span-type ID, time window, and attribute filters.
There are no service-specific fields, operation names, or datasets in the
native feature.

Selecting a span type collapses the catalog. **Change span**, beside the single
selected-span heading, reopens the list; choosing another type collapses it again.

It supports aggregate S3 scopes and single raw traces. The Rust backend
computes the entire analysis through `GET /api/span-tail` (aggregate scope)
or `POST /api/span-tail` (raw bytes). It reuses credential-scoped resolve,
fold, and Parquet reads, discovers numeric attributes and producer units,
and identifies categorical attributes with up to twelve values. The browser
only requests reports and renders summaries, density cells, and trace links. It compares
the faster half with the selected slow percentile or histogram band. Choosing a
categorical group recalculates the percentiles inside that group. Value-scoping
controls sit beside their property graph; the active field/value and an
**All instances** reset stay available if filtering removes that graph.
Orange dashed plot lines label the percentile cutoff, or both endpoints of a
brushed range. The comparison dropdown displays **Custom selection: X to Y**;
choosing a percentile clears the brush and restores that percentile comparison.

The view starts with:

- Explicit comparison-group definitions, counts, and source-file coverage.
- Categorical latency distributions when a varying categorical field is present.
  **Stacked** is the initial view: distinct colors form cumulative
  layers on one graph with a white total curve. **Overlay** remains selectable.
  Each value shows its count and p99. Shared logarithmic bins and Y scales make
  the shapes comparable. Overlay heights are percentages within each value;
  stacked layers use exact bin counts divided by the full recorded population,
  so their heights add up to the pooled total distribution.
  Missing values are counted separately. A sole property is a plain heading;
  buttons switch fields when several properties are available.
  Paired bars below the distributions show typical and slow value proportions,
  with the same colors, actual cohort counts, and explicit missing values.
- A **Numeric fields** section directly below the property graphs, with three
  compact plots ordered by group-distribution differences. Density cells count
  every loaded recorded value; outlined dots are trace-linked samples.
  Blue marks other spans and orange marks slow or selected spans, including
  exact color splits where cutoffs cross a cell. Brightness uses the same count
  scale for both hues; density counts cover whole cells even at color splits.
  Elapsed time uses shared backend bins with a zero-inclusive first bin and
  an endpoint at the full scoped population's exact observed maximum.
  A zero-only population uses a one-nanosecond display width.
  **Elapsed axis** switches numeric plots to a linear axis; the same choice
  applies to compact and enlarged plots.
  The backend shares value limits across identical units and normalizes declared
  time units to milliseconds. Unknown units remain separate. Additional fields
  and enlarged plots are available on demand.
- Two stacked time-composition bars for typical and selected spans, using the
  existing On CPU, Blocked, Async wait, Scheduling delay, and Unknown categories.
  Rust computes the mean per-instance shares and category durations. Mean elapsed
  time appears beside each bar; hover a segment or legend item for its values.

Categorical bin counts, normalization, and exact per-value percentiles are
computed in Rust across the selected source scope, including any attribute or
group filter. A slow-percentile or histogram-band comparison does not discard
the rest of the distribution. Fields constant within that scope or exceeding
twelve values are omitted. Explicit grouping by a field with more than twelve
values returns an error rather than a partial cohort. The backend screens for distribution or p99
differences with sample-size and coverage thresholds. Screening counts and
definitions are available on demand; an excluded field can have no clear
difference in this sample or insufficient observations. Group counts remain
visible because a small sample's observed p99 is a weak basis for a conclusion.

Numeric comparisons use every recorded value to find the largest difference
between the two cumulative distributions at any value threshold, including
ties. The backend subtracts a descriptive sampling margin that accounts for
cohort sizes and the number of candidate fields. The remaining gap determines
ranking. Fields with fewer than twenty recorded values in either group follow
adequately observed fields; unavailable comparisons remain explicit.
Coverage warnings are separate from this ordering. Duration attributes that repeat
total elapsed time and constant fields are omitted. The backend's ordering is
descriptive, not a significance test.

Elapsed aliases require declared time units and use a tolerance of 1 ns or
0.2% of elapsed time, whichever is larger. A timer that matches every recorded
value is omitted even when some values are missing. A near-duplicate with more
than 98% overall matches is omitted only when every recorded value in both the
source/group's fixed p99 tail and the current selection matches. Each checked
cohort must be nonempty and have at least twenty recorded values or complete
recorded coverage. A near-duplicate can reappear when a custom selection exposes
differences or lacks enough recorded coverage; a meaningful rare-tail difference
must remain visible. Unknown units never imply an elapsed alias.

Each card shows group means, a qualitative tail-difference label, and repetition
across qualifying source files. Click its difference badge for a table of
recorded counts, coverage, means, medians, and p99s. The popup explains the CDF
gap, sampling margin, and conservative lower bound used for ordering; labels
are descriptive cutoffs, not significance claims. Spearman across recorded values
correlation is a secondary expandable detail. Coverage warnings remain separate;
Escape closes the explanation and returns focus.
File repetition needs five observations in each cohort. Spearman compares up to
2,048 evenly spaced observations across latency ranks; comparison means,
medians, and p99s use every loaded recorded value in their original units.
Drawing retains up to 330 rank/tail span samples and adds representatives for
occupied numeric density cells with navigable source identity. The 37-by-32
grid bounds drawing at 1,514 actual recorded-span dots per numeric field;
density counts still cover every loaded recorded value. Hover or keyboard focus
shows one actual retained span and its original key/value strings, including
duplicate keys.
**Box X/Y** gives the originating density box's elapsed and field bounds,
units, and full span count;
**Sample X/Y** gives the shown span's own coordinates. White outlines connect
the box and actual sample to these details without changing their cohort colors.
Moving within or between cells updates the span and box context. Current trace
reports supply an in-cell span for each occupied box with source identity.
Older cached or count-only reports can lack such a span: the inspector explicitly
labels a nearby retained span or unavailable sample. No metadata is inferred
from the active field.

Click a dot or populated cell, or press Enter or Space, to pin the details.
Only one inspector is active at a time. Pinning preserves its original values
for reading, text selection and copying while the page scrolls or resizes;
**Unpin** or Escape clears it. The inspector docks outside the field or enlarged
dialog, keeping that field's controls accessible. Short windows retain bounded
scroll access. Closing an enlarged plot or replacing the report releases its
inspector. **Open trace** is the explicit navigation action and preserves the
source and credential contract.
Older cached reports show an explicit sample-only warning and do not invent
missing density or shared-unit metadata. Their elapsed axis ends at the largest
retained sample or visible selection bound, rather than an inferred full-data maximum.

Recomputation blurs and disables previous results. Failure and cancellation
clear stale comparisons; replacing the source retires its previous detail.
An unchanged selection does not restart a failed or cancelled request;
**Analyse source traces** retries it explicitly. New selections start at the
top of the detail panel, while analysis completion preserves the scroll position.
The paired comparison replaces the catalog's single time bar once analysis is
available. Flamegraph links and recorded-instance browsing share one trace
inspection section beneath the charts.

The shareable URL preserves property/value scope, percentile or duration band,
displayed property, stacked/overlay mode, numeric elapsed scale and the expanded
field list. Refresh and Back/Forward restore those controls and their analysis;
display-only changes do not request another report. **All instances** clears
the grouped scope. See [the URL contract](../../URL_CONTRACT.md) for the parameter
names and defaults.

## Run locally

Build the canonical UI, then start the normal viewer from the repository root:

```bash
cd dial9-viewer/ui
npm ci
npm run build
cd ../..
cargo run --release -p dial9-viewer -- serve --local --dev --port 3003
```

Open `http://127.0.0.1:3003/span_explorer.html` with a trace or S3 source.
The analysis uses the viewer's existing source and credential configuration.
Keep local trace captures outside the repository.

## Limits and interpretation

The analysis reads at most twenty-four files in the backend's deterministic
resolve order and retains at most one million matching spans. Partial
coverage and field-discovery limits are explicit. Exact span-type IDs
separate callsites with the same name. Occurrence-time filtering uses
`end_ns` in the same half-open interval as the catalog. Each comparison
request reads cached Parquet after any required folds.

Point metadata is hydrated in a second pass over only the sources containing
bounded examples. Aggregate requests reuse cached Parquet; raw requests decode
the source again after releasing the full observation collection. The join
uses source identity, span identity, and exact time bounds; ambiguous matches
remain unavailable. This avoids retaining full key/value records for every
loaded span. Each example includes at most 64 whole pairs and 8 KiB of serialized
attribute arrays, with a 16 MiB budget across all emitted references. Omitted
pair counts are explicit, and values are never clipped into plausible strings.
Null or absent attributes mean unavailable metadata; an empty array with zero
omissions means no recorded fields.

Time composition uses the same estimated five-way accounting as the catalog.
Each positive-time instance contributes equally to the bar's shares; category
durations are means across all instances in that cohort. Missing composition
contributes Unknown rather than disappearing from the comparison. A zero-time
instance contributes no share. These estimates describe the recorded time,
not the resource or cause of a wait. Producer timers can overlap and are not
added together. Producer-declared units are
preserved in an additive `attribute_units` Parquet column. Older
parts without that column show unknown units; conflicting units exclude
the field instead of guessing a conversion.

Nearest-rank thresholds include all boundary ties. Bands include both
duration endpoints. A band that includes the faster half produces
overlapping comparison groups. Missing numeric values are excluded from
that field's mean and comparison. None of these associations establishes
causation or predicts the improvement from changing a field.

## Verification

```bash
cd dial9-viewer/ui
mise exec node@22 -- npm run test
mise exec node@22 -- npm run check:types
mise exec node@22 -- npx tsc --noEmit
mise exec node@22 -- npm run build
cd ../..
cargo fmt --check
cargo clippy --all-targets --all-features
cargo nextest run
cargo nextest run --stress-duration 20s
cargo build --release -p dial9-viewer --bin dial9-viewer
```

Rust backend tests cover discovery/ranking, latency aliases, constant and missing
values, tied comparisons, group-specific thresholds, per-file agreement,
equal-weighted time composition, missing/zero composition, categorical bin
boundaries, unequal group sizes, zero/extreme durations, scoped category
distributions, and raw/Parquet parity.
Browser verification should cover multiple span types and raw/aggregate
sources, group/percentile/band changes, trace jumps, cohort details, recorded
point fields, and desktop/mobile layout.
