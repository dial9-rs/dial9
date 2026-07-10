# Migration ledger

Retired / amended / added inventory rows, one line per entry (shared format,
chunk-1 header): `<feature-row-id> | retired|amended|added | <ticket> | <reason>`.
Maintainer sign-off = approving the PR that edits the ledger line.

features/03 census +#d9-ui-switch | added | T13 | dual-UI switch control (T38) renders on BOTH page generations; the census diff shows it CHANGED between sides by design (label "Switch to new UI" vs "Switch to legacy UI", href legacy<->new with ui=legacy pinned toward legacy)
features/03 F4/F5/F6/F166 | amended | T13 | exact-mode fetch+gunzip+parse now runs inside the T16 worker load pipeline on the migrated page (parse off the main thread); same stream/buffered selection, loading-phase labels (F9), byte-identical parse and error surface - mechanism anchor moves from the inline bootstrap to lib/trace load.ts
features/01 census: #d9-ui-switch | added | T14 | Dual-UI switch control (T38). Registering the browser page in NEW_UI_ENTRIES makes the always-visible control render on BOTH sides, with per-side label/href ("Switch to new UI" -> /new/index.html vs "Switch to legacy UI" -> /index.html?ui=legacy). The old-vs-new affordance-census diff therefore carries exactly one CHANGED entry for #d9-ui-switch; everything else is zero-diff.
