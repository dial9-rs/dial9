# T21 - Viewer shell - HANDOFF

(Replaces the T20 HANDOFF inherited through the branch chain; T20's record
lives in the tree at base d4d67c8.)

STATUS: gates-green; DoD live-walks PENDING (deferred to trailing audit).

## Completed (commits on ticket/T21-viewer-shell off d4d67c8)
- 71c3cae scaffold: lit-html dep + track-layout catalogue
- 74ba554 shell state: store, toasts (U), esc-cascade mechanism
- 4ba5e2a shell: layout, chrome, help, tracks, entry (src/pages/viewer/)
- 7e7daa9 main landmark (drop role=application) + shell verify script
- 89db5d5 wip salvage: toasts/esc-cascade/track-layout tests (orchestrator
  salvage after a connection-drop death)
- 8774ec7 fix toasts test fake: live className getter/setter, drop Proxy
- (this) test(T21) extend ui-switch registry expectation for viewer.html

Session note: the implementer agent died repeatedly on connection drops
(unstable API window). The last three commits were completed by the
orchestrator session: two are TEST corrections (the toasts fake's Proxy
broke node identity so hide()/clear() never removed children - impl was
correct; the ui-switch registry test's own comment mandates the one-line
extension when a page registers) and none touched shell IMPLEMENTATION.

## Gate evidence (all green)
- `npx tsc --noEmit`: exit 0
- `npm run test` (full suite): 1016 passed / 1 expected-fail / 11 skipped,
  0 unexpected
- `npm run build`: clean, 17 static-copy items, new/viewer.html entry bundled
- `cargo build -p dial9-viewer`: clean (rust-embed picks up the new entry)

## DoD live-walks - PENDING (trailing audit obligation)
Deferred (unstable environment made a live dev-server + parity session
risky). Must run before T21 is fully DoD-complete:
- row-walker on owned A/T/U rows against the shell (dial9-viewer/ui/parity/)
- axe scan clean on the shell (palette/help/landmarks)
- tab-order dump toolbar->minimap->tracks->inspector

The shell renders placeholder track slots by design (track CONTENT = T22-T30);
the walks verify the A16 ARIA live region, help overlay (T), toasts (U), and
the K6 tab order. Recorded in execution-plan.md audit log as a trailing item.

## Open questions
None blocking.
