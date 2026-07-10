# Migration contract ledger

One line per consciously retired/amended/added feature-surface entry
(shared format, chunk-1 header): `<feature-row-id> | retired|amended|added |
<ticket> | <reason>`. Maintainer sign-off = approving the PR that edits the
ledger line.

features/01 census: #d9-ui-switch | added | T14 | Dual-UI switch control (T38). Registering the browser page in NEW_UI_ENTRIES makes the always-visible control render on BOTH sides, with per-side label/href ("Switch to new UI" -> /new/index.html vs "Switch to legacy UI" -> /index.html?ui=legacy). The old-vs-new affordance-census diff therefore carries exactly one CHANGED entry for #d9-ui-switch; everything else is zero-diff.
