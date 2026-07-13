# T45 - Surface segment metadata in the viewer (#68) - HANDOFF

## STATUS: DONE (DoD met, all gates green)

Surfaces the trace-EMBEDDED `service`/`host` from `ParsedTrace.segmentMetadata`
in the viewer toolbar file-info area (features/02 C1a), reconciled against the
S3-key-derived `svc`/`host` URL params. Closes the read side of #68 (the issue
itself is closed via T44 per the ticket; no GitHub action taken here).

## COMPLETED (commit shas, on branch ticket/T45-segment-metadata)

- `73f40e2` feat(viewer): pin SegmentMetadata service/host read contract (T45)
  - `src/types/trace.d.ts`: `SegmentIdentity` interface (the pinned read-contract shape).
  - `src/lib/trace/segment-metadata.ts` (NEW): `SEGMENT_SERVICE_KEY`/`SEGMENT_HOST_KEY`
    (literal `"service"`/`"host"`), `readSegmentIdentity(trace)`,
    `readKeyDerivedIdentity(search)`, `reconcileIdentity(embedded, keyDerived)`
    (+ `IdentityField`/`ReconciledIdentity`).
  - `src/lib/trace/index.ts`: barrel exports.
  - `src/lib/trace/segment-metadata.test.ts` (NEW): 13 tests - key contract,
    accessors, reconciliation rule, and a row-walk over the demo trace + a T42
    window fixture.
- `3bcd86c` feat(viewer): surface trace service/host in toolbar file info (T45)
  - `src/pages/viewer/toolbar.ts`: `keyDerivedIdentity?` added to `ToolbarDeps`;
    `fileInfoTemplate` renders `identityTemplate` chips (`[data-file-identity]`,
    `[data-identity="service"|"host"]`) with the embedded-wins + tooltip rule.
  - `src/pages/viewer/main.ts`: reads `readKeyDerivedIdentity(location.search)`
    once and passes it into the shell/toolbar deps.
  - `src/styles/viewer.css`: minimal chip styling.
- `cf0e49c` docs(T45): C1a inventory row + ledger addition for #68
  - `docs/ui-inventory/features/02-viewer-html.md`: new C1a row.
  - `docs/tickets/ledger.md`: `features/02 C1a | added | T45 | ...` line.

## KEY FINDINGS / DECISIONS

- **Writer key names confirmed literal (`"service"` / `"host"`)** - no STOP-gate
  needed. Confirmed across: `dial9-utils/src/s3.rs` S3 source
  `.metadata("service"|"host", ...)`; `dial9-viewer/src/bin/gen_fixtures.rs`
  `standard_metadata()`; the `dial9-core` writer roundtrip test; the
  `metrics-service` example (`service` only). SegmentMetadata is a free-form KV
  map; the key names are a writer-side convention, now pinned once in
  `lib/trace/segment-metadata.ts` with the shape in `types/trace.d.ts`.
- **Demo trace carries `service` (= "metrics-service") but NOT `host`** - the
  metrics-service example emits no host. The host surface is row-walked via the
  committed T42 fixture `parity/fixtures/segments/window-00.bin.gz`
  (`service`="svc-fix", `host`="window"). No new fixture needed.
- **Two sources reconciled** (embedded metadata vs URL `svc`/`host` from S3-key
  parsing via `traceTitleParams`, which the browser page appends when opening
  the viewer): embedded WINS when both present; a disagreeing key-derived value
  is tooltipped; key-derived shows as a fallback when the trace has no embedded
  metadata. This also restores the structured-metadata display the T33 C1 port
  had silently dropped (legacy C1/W11).
- **Frozen core untouched** - `trace_parser.js` read-only; the boundary guard
  `check:boundary` passes (module lives inside `lib/trace`).

## REMAINING

None. Scope fence respected (contract addition + toolbar surface + one
inventory row + ledger). #68 closure itself is T44's per the ticket.

## BLOCKERS / QUESTIONS

None.

## EVIDENCE (all gates green)

- `npx tsc --noEmit` -> exit 0 (no output).
- `npm run build` -> clean; `built in 453ms` (the `fs`/`os`/`child_process`
  externalized-for-browser warnings are pre-existing from the frozen
  `trace_parser.js` Node imports, unrelated to this change).
- `npm run test` (full Vitest) -> `Test Files 94 passed | 1 skipped (95)`,
  `Tests 1478 passed | 1 expected fail | 11 skipped (1490)`. No unexpected
  failures; no straggler timeouts (single full-parallel run). `check:boundary`
  (pretest) -> OK.
- `cargo build -p dial9-viewer` -> exit 0 (`Finished dev profile`).
- Focused: `npx vitest run src/lib/trace/segment-metadata.test.ts
  src/pages/viewer/toolbar.test.ts` -> 20 passed.
