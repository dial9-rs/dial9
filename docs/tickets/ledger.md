# Migration ledger

Retired / amended / added inventory rows, one line per entry (shared format,
chunk-1 header): `<feature-row-id> | retired|amended|added | <ticket> | <reason>`.
Maintainer sign-off = approving the PR that edits the ledger line.

features/03 census +#d9-ui-switch | added | T13 | dual-UI switch control (T38) renders on BOTH page generations; the census diff shows it CHANGED between sides by design (label "Switch to new UI" vs "Switch to legacy UI", href legacy<->new with ui=legacy pinned toward legacy)
features/03 F4/F5/F6/F166 | amended | T13 | exact-mode fetch+gunzip+parse now runs inside the T16 worker load pipeline on the migrated page (parse off the main thread); same stream/buffered selection, loading-phase labels (F9), byte-identical parse and error surface - mechanism anchor moves from the inline bootstrap to lib/trace load.ts
features/01 census: #d9-ui-switch | added | T14 | Dual-UI switch control (T38). Registering the browser page in NEW_UI_ENTRIES makes the always-visible control render on BOTH sides, with per-side label/href ("Switch to new UI" -> /new/index.html vs "Switch to legacy UI" -> /index.html?ui=legacy). The old-vs-new affordance-census diff therefore carries exactly one CHANGED entry for #d9-ui-switch; everything else is zero-diff.
features/03 F147/F153 | amended | T19 | zoom->URL sync on the migrated page is debounced (150ms, one replaceState per zoom burst) and additionally carries the versioned view-state hash (docs/ui-inventory/05-url-view-state.md); legacy worker-zoom/offworker-zoom query semantics unchanged (F148/F149/F151/F152 intact - differ J5/J9 zero-diff, restore-on-load writes nothing)
features/03 census +.d9-copy-link | added | T19 | copy-link button in the migrated page header (both modes): flushes the pending debounced view-state write, copies location.href; minimal share affordance until chunk-2's status bar chrome
features/03 F186 | added | T20 | unified `?` help overlay on the migrated flamegraph page, both modes (K3: ? was verified dead on legacy); F94 reference table is the content baseline; widget's own [info] overlay untouched
features/03 F187 | added | T20 | `f` fit: reset zoom to full tree preserving the search query (K4; the Escape cascade clears search first, F44 forbids that for fit); composed from released widget behaviors, frozen core untouched
features/03 F188 | added | T20 | `z` zoom undo over a bounded page-owned view history (K4); deliberately NOT URL state (05-url-view-state.md); name-path fidelity equals the F150 URL restore
features/03 F157 | amended | T20 | migrated page: an OPEN unified `?` overlay closes ahead of cascade stage 1; cascade byte-identical when it is closed
features/01 A8 | added | T20 | unified `?` help overlay on the migrated browser page (K3; NOTE: no legacy browser help surface exists at HEAD - the ticket's "exists" and K3's browser claim did not re-verify)
features/01 A9 | added | T20 | `/` focuses the active tab's search input (K3 one-vocabulary; focus-tolerant router, K5)
features/01 F21 | added | T20 | keyboard window selection on the heatmap (K2 fix): focusable plot (axe-serious scroll region fixed), Shift -> arrows -> Enter/Shift confirm through the same finalizeSelection as F12, Escape cancels, announced
features/01 D9 | amended | T20 | migrated page: Enter in any controls-bar field runs the search under the same enabled gate (K2 "no key submits search")
features/01 F12 | amended | T20 | cross-reference only: the new keyboard path (F21) lands on the same finalizeSelection; mouse behavior unchanged
