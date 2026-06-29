# ADR-0003: The folded-set is the output listing, not a manifest

- **Status:** accepted
- **Date:** 2026-06-22

## Context

The aggregation pipeline is moving from batch pre-aggregation to
demand-driven, progressively-refined sampling (see [CONTEXT.md] —
"Refinement loop"). At query time the backend must answer two questions
cheaply and correctly:

- **What files match this scope?** (`files_matched` — the coverage
  denominator and the input to the [order key].)
- **Which of those have already been folded into the `samples` table?**
  (`files_folded` — so the refinement loop fetches each immutable source
  file at most once.)

The POC answered "folded?" with an append-only `_manifest/` of
`source_key`s plus a skip-set read at the start of each run. That is a
second structure to keep in sync with `samples`, and the prior S3 ingest
bug (manifest written locally, read from S3 → skip-set a no-op, every
re-run duplicated rows) came directly from that split.

A tempting simplification — derive the folded-set from the `samples`
table itself (`SELECT DISTINCT source_key`) — is **wrong**: a source file
that decodes successfully but contains zero CPU samples produces no rows
in `samples`. It would look unfolded forever and be re-fetched
(~37 MB GET + decode) on every poll. The pipeline is network-bound, so
that is exactly the redundant work the design exists to avoid.

## Decision

There is no manifest and no skip-set. **The presence of an output
part-file is the record that its source file has been folded.**

- Each source file folds to a deterministically-named part-file:
  `samples/service={svc}/date={YYYY-MM-DD}/host={host}/{blake3(source_key)}.parquet`.
  The scope columns are derived from `source_key` (as `parse_source_key`
  already does); the BLAKE3 hash of the full `source_key` is the leaf.
- A file with **zero CPU samples still writes an (empty) part-file**, so
  "folded, nothing here" is recorded and the file is never re-fetched.
- `files_folded` = a **scope-pruned LIST** of the partitioned `samples/`
  tree. `files_matched` = a LIST of the source scope. Coverage is their
  intersection.
- Re-folding an immutable source file writes the **same key**, so re-runs
  are idempotent with no skip-set.

The hash lives only in the leaf (not the path) so the folded-set LIST
stays prunable by scope — a flat `samples/{hash}.parquet` layout would
force a full-history, fleet-wide LIST (the unbounded-list failure mode
that already bit ingest) plus an in-memory intersection. Partitioned
paths also give DataFusion Hive partition pruning on the query side for
free.

## Consequences

- Two structures (`_manifest/` + skip-set) collapse into one (the output
  listing). Fewer moving parts, and the completion record can no longer
  drift from the data — they are the same object.
- Completion is crash-consistent: a part-file exists iff the fold
  finished. A crash mid-write leaves no part-file, so the file is simply
  re-folded later (idempotent).
- Empty part-files add a small number of tiny objects (only for
  zero-sample files, which are rare). Acceptable versus re-fetching them
  forever.
- The output tree is namespaced by `SAMPLES_FORMAT_VERSION`
  (`{output_prefix}/v{N}/samples/…`); a format bump points at a fresh
  empty tree that repopulates lazily.
- `ORDER_VERSION` is NOT part of any output path — the folded part-files
  are order-independent and must survive an ordering change untouched.

[CONTEXT.md]: ../../CONTEXT.md
[order key]: ../../CONTEXT.md
