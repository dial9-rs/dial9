# Migration ledger

Retired / amended / added inventory rows, one line per entry (shared format,
chunk-1 header): `<feature-row-id> | retired|amended|added | <ticket> | <reason>`.
Maintainer sign-off = approving the PR that edits the ledger line.

features/03 census +#d9-ui-switch | added | T13 | dual-UI switch control (T38) renders on BOTH page generations; the census diff shows it CHANGED between sides by design (label "Switch to new UI" vs "Switch to legacy UI", href legacy<->new with ui=legacy pinned toward legacy)
features/03 F4/F5/F6/F166 | amended | T13 | exact-mode fetch+gunzip+parse now runs inside the T16 worker load pipeline on the migrated page (parse off the main thread); same stream/buffered selection, loading-phase labels (F9), byte-identical parse and error surface - mechanism anchor moves from the inline bootstrap to lib/trace load.ts
features/03 F147/F153 | amended | T19 | zoom->URL sync on the migrated page is debounced (150ms, one replaceState per zoom burst) and additionally carries the versioned view-state hash (docs/ui-inventory/05-url-view-state.md); legacy worker-zoom/offworker-zoom query semantics unchanged (F148/F149/F151/F152 intact - differ J5/J9 zero-diff, restore-on-load writes nothing)
features/03 census +.d9-copy-link | added | T19 | copy-link button in the migrated page header (both modes): flushes the pending debounced view-state write, copies location.href; minimal share affordance until chunk-2's status bar chrome
