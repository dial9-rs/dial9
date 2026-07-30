# Viewer UI state authority

- **Status:** proposed
- **Date:** 2026-07-27
- **Scope:** all migrated viewer pages: browser, trace viewer, flamegraph,
  Tokio Stats, and Span Explorer
- **Amends if adopted:** ADR-0004 sections 2 and 4; N17 in
  `docs/ui-inventory/02-architecture.md`

## Decision

The direction is correct, but "put absolutely all state in one store" is the
wrong invariant.

The invariant should be:

> Each browser document has exactly one authority for every meaningful UI
> value. The authority is changed only by semantic intents. URL, storage, DOM,
> canvas, widgets, and asynchronous work are projections or Adapters, never
> peer authorities.

A value is meaningful when it can affect any of:

- rendered output;
- which operations are enabled or what the next input means;
- request scope or whether an asynchronous result is accepted;
- navigation, Back/Forward, Copy Link, or persisted preferences;
- reproduction of the same analysis from the same source.

The current first-party store centralizes some values, but it does not establish
authority. Its public `update(slice, patch)` Interface allows arbitrary writes,
cannot make cross-slice transitions atomic, and cannot detect meaningful state
held by the DOM, a widget, or a module closure. Deep linking exposed this
problem; it did not create it.

Adopt a small Redux/Elm-style State Authority Module with:

1. a private model;
2. a pure reducer over semantic intents and effect results;
3. one atomic commit per intent;
4. typed Views scheduled on chrome, content-canvas, and overlay lanes;
5. effects behind Adapters, with result identity carried back through the
   reducer;
6. an exhaustive policy for every model field;
7. architecture checks that prohibit alternate authorities.

Use one authority per loaded MPA document, not one JavaScript singleton across
pages. The runtime and rules are shared; the page models are discriminated and
page-specific.

Do not adopt Redux Toolkit or XState as the whole application model. Redux's
reducer discipline is the useful part, but a Redux package would not prevent
DOM- or widget-owned state. Toolkit's default serializability and immutability
assumptions are also a poor match for very large parsed traces containing
`Map`, `Set`, and frozen-core objects. XState is useful for individual load and
stream lifecycles, but expressing the entire analytical view as a statechart
would add more Interface than leverage. Keep the first-party runtime, and give
it the constraints that made the old Redux architecture effective.

## Research

### Repository audit

The PR currently has three distinct write models: store-owned values,
DOM/module-owned values, and widget-owned values.

| Surface | Current authorities | Consequence |
| --- | --- | --- |
| Browser | Store plus uncontrolled inputs and checkboxes | URL serialization and raw selection can read the DOM rather than the store (`pages/browser/actions.ts`; the exception is documented in `pages/browser/state.ts`). |
| Trace viewer | Store plus load controller state, modal/search state, zoom history, drag/copy feedback, and toasts | Some local values are valid resources; others affect output or future behavior and are unclassified alternate authorities. |
| Flamegraph exact mode | Frozen widget plus a store that samples `getViewState()` | The store mirrors the authority for URL sync; it is not the authority (`pages/flamegraph/view-state.ts`). |
| Flamegraph API/diff modes | DOM controls, module variables, stream state, minimap state, and widget state | Most meaningful state bypasses the shared store (`pages/flamegraph/api-mode.ts`, `diff-mode.ts`, and `minimap.ts`). |
| Tokio Stats | Module-level periods, active tab, max-files value, DOM threshold/timezone, and request lifecycle | No document-level model currently owns the page (`pages/tokio-stats/main.ts`). |
| Span Explorer | Module-level selection, duration band, filters, sort, stream lifecycle, column overrides, and DOM time fields | No document-level model currently owns the page (`pages/span-explorer/main.ts`). |

The recent `VIEWER_STATE_OWNERSHIP` matrix is useful but only exhaustive over
fields already admitted to `StoreState`. It cannot fail when a new module-level
`let`, DOM value, or widget field is introduced. The flamegraph mirror is the
clearest counterexample: a field can be exhaustively classified in the store
while another object remains authoritative.

The current store also has transition-level weaknesses:

- `update(slice, patch)` describes storage layout, not user intent.
- A caller can create a state that violates cross-field invariants.
- One user operation that changes several slices produces several commits.
- Effects and their results have no common identity protocol.
- Subscriber-triggered updates make ordering depend on registration and later
  frames.
- RAF-coalescing is fixed at store-notification level, although DOM chrome,
  content canvas, pointer overlay, and settled URL writes need different
  schedules.

### Relevant prior art

The useful parts of Redux are a single store, reducer-only writes, event-like
actions, atomic dispatch, and replayable transitions. Those properties are more
important here than the Redux library.

The Elm Architecture adds the missing effect model: `update(model, message)`
returns the next model plus commands, and command results return as messages.
That is a good fit for trace loading, HTTP, SSE, clipboard, and URL work.

Controlled-input architecture treats DOM values as rendered projections and
DOM events as proposed intents. The same rule must apply to the frozen
flamegraph widget: render canonical state into it and treat callbacks or
`getViewState()` as proposals only.

Statecharts are valuable for the internal Implementation of operations such as
`idle -> loading -> ready | failed`, especially when cancellation and stale
completion matter. They should remain internal reducer helpers, not become the
application's external Interface.

References:

- Redux, "Three Principles":
  <https://redux.js.org/understanding/thinking-in-redux/three-principles>
- Redux Style Guide: <https://redux.js.org/style-guide/>
- Elm Architecture: <https://guide.elm-lang.org/architecture/>
- XState concepts: <https://stately.ai/docs/xstate>

## The state boundary

The authority owns meaningful state, not every mutable byte in the process.
Use this test:

> If an Adapter is destroyed and reconstructed from the model, can the user
> observe a difference now, or can the same next intent produce a different
> result?

If yes, the missing value belongs in the model or must be derivable from it. If
no, it is implementation state and may remain local.

| Class | Authority treatment | Examples |
| --- | --- | --- |
| Durable analytical view | Model; URL policy required | viewport, semantic selection, filters, sort, active analysis, flamegraph zoom |
| Persisted preference | Model; storage policy required | timezone, panel folds, widths, track order |
| Session state | Model; explicitly excluded from URL/storage | active modal, zoom undo history, selected tab when intentionally non-shareable |
| Interaction state | Model or a reducer-owned interaction machine | drag, keyboard selection, meaningful hover, pending gesture |
| Operation state | Model, including request identity | loading/progress/error, stream coverage, pending parse, copy status |
| Immutable resource | Model stores identity and availability; Adapter may hold bytes/object | parsed trace, `File`, `ArrayBuffer`, worker result |
| Derived data | Selector/cache, never a second source of truth | filtered rows, flamegraph tree, layout, formatted timestamps |
| Mechanical resource | Adapter-local | DOM nodes, canvas contexts, RAF/timer IDs, `AbortController`, pointer capture |
| Secret capability | Secret Adapter holds bytes; model holds redacted status/reference | AWS credential draft or token |

An immutable parsed trace can remain directly in the model during migration.
Longer term, a `TraceRef` plus a private resource registry avoids accidental
DevTools serialization and model copies. The registry is not another authority:
resources are immutable, and only the model decides which reference is active.

Credential bytes are the deliberate exception to replayable state. Keep them in
a narrow credential Adapter and expose only presence, validation, and operation
status to the model. Intents and transition journals must be redacted by type,
not by a best-effort string filter.

## Interface designs considered

Four independent designs were compared.

### A. Minimal session

`mountDial9Ui`, `command`, and `dispose` put the whole page behind a very small
Seam. This has excellent Depth and makes accidental state reads difficult, but
it hides too much for canvas hit testing, focused rendering, and diagnostics.
The implementation would grow private escape hatches.

### B. Flexible authority

`initialize`, typed `read`, typed `observe`, `dispatch`, coalesced `offer`,
`checkpoint`, and `dispose` support all current scheduling and integration
needs. It has the most flexibility, but checkpoint and initialization details
make the external Interface wider than common callers need.

### C. Binding-oriented kernel

`snapshot`, `dispatch`, and `connect(binding, adapter)` give ordinary UI authors
a safe default: a binding owns projection, event translation, scheduling, first
render, and disposal. This provides strong Leverage and Locality, but a binding
DSL would itself be a substantial new framework and is premature before the
state model is proven.

### D. Ports-and-Adapters program

`boot`, `evolve`, and `project` make the pure application program explicit and
put all browser/remote work behind ports. This is the cleanest test Seam, but
making DOM and every browser primitive a public port would create shallow
Adapters and too much configuration.

### Recommended hybrid

Use D's pure transition shape internally, B's typed Views for rendering, and A's
small external Seam. Do not build C's binding DSL yet.

```ts
type RenderLane = "chrome" | "content" | "overlay";

interface Step<Model, Effect> {
  readonly model: Model;
  readonly effects: readonly Effect[];
  readonly invalidated: ReadonlySet<ViewId>;
}

interface UiProgram<Model, Event, Effect> {
  initialize(facts: BootFacts): Step<Model, Effect>;
  evolve(model: Readonly<Model>, event: Event): Step<Model, Effect>;
}

interface View<Page, Value> {
  readonly id: string;
  readonly lane: RenderLane;
  readonly select: (model: Readonly<PageModel[Page]>) => Value;
}

interface UiAuthority<Page extends PageId> {
  read<Value>(view: View<Page, Value>): Readonly<Value>;
  observe<Value>(
    view: View<Page, Value>,
    render: (value: Readonly<Value>, revision: number) => void,
  ): () => void;
  dispatch(intent: IntentByPage[Page]): DispatchResult;
  offer(signal: FrameSignalByPage[Page]): void;
  dispose(): Promise<void>;
}
```

`createUiAuthority(pageProgram, adapters, bootFacts)` performs initialization.
Page modules receive only the `UiAuthority` Interface. They never receive the
model, reducer, effect runner, history object, or writable store.

`read` accepts only a declared View, not an arbitrary selector. This keeps
dependencies inspectable while supporting hit testing and command enablement.
`offer` is restricted to latest-only frame signals such as pointer movement.
Gesture completion drains the pending signal before dispatching the terminal
intent.

## Model shape

Each page owns a discriminated model with the same top-level roles:

```ts
interface PageModel<ViewState, InteractionState> {
  readonly source: SourceState;
  readonly view: ViewState;
  readonly preferences: PreferenceState;
  readonly interaction: InteractionState;
  readonly operations: OperationState;
  readonly route: RouteSyncState;
}
```

These roles are not independently writable slices. They are organization inside
one atomic value. Private page reducers may be composed, but the root reducer
validates and commits the complete transition.

Use semantic anchors rather than frozen-core object references:

- `{ taskId }`, not a task object;
- `{ spanId }` or `{ taskId, startNs }`, not a `PollSpan`;
- `{ startNs, endNs }`, not a canvas selection object;
- a frame-name path, not a flamegraph tree node.

Use discriminated states instead of sentinel values:

```ts
type Viewport =
  | { kind: "unresolved" }
  | { kind: "full"; minNs: number; maxNs: number }
  | { kind: "window"; minNs: number; maxNs: number; startNs: number; endNs: number };

type Operation<Progress, ResultRef> =
  | { status: "idle" }
  | { status: "running"; id: string; sequence: number; progress: Progress }
  | { status: "ready"; id: string; result: ResultRef }
  | { status: "failed"; id: string; error: UiError };
```

This prevents placeholder values such as an all-zero viewport or an empty array
that ambiguously means "not loaded", "loaded empty", or "failed".

## Transition and effect rules

Every event executes in this order:

1. dequeue one intent or effect result from a non-reentrant FIFO;
2. reduce it against one model revision;
3. validate root invariants;
4. commit exactly one new revision;
5. schedule invalidated Views;
6. start declared effects;
7. return effect results through the same event queue.

Observers never update the model. Rendering is a terminal projection. A
render-time discovery that should change state is an architectural error; the
derivation belongs in the reducer or a selector.

Every asynchronous effect receives an identity. Result events carry that
identity and, for streams, a monotonic sequence/coverage value. The reducer
ignores results that no longer match the active operation. `AbortController`,
Worker, timer, and EventSource handles remain private to the effect Adapter.

Effects start only after commit. A failed external effect produces a typed
result event and explicit operation state. Programmer invariant failures throw
in development.

## Scheduling

State commits are synchronous. Projection delivery is scheduled per View:

- **chrome:** microtask-coalesced;
- **content:** at most once per animation frame;
- **overlay:** independent animation frame, latest-only;
- **URL/storage:** settled/debounced effect.

When content and overlay are both invalidated, content paints first. Overlay
delivery performs no DOM writes. This retains N1/N2 performance without making
all subscribers wait for the same RAF.

High-frequency raw browser events are translated by an interaction Adapter.
The Adapter may retain the latest uncommitted pointer sample as a mechanical
resource, but `pointerup`, `keyup`, blur, and cancellation must flush it through
`offer` before dispatching their terminal intent.

## Controlled Adapters

### DOM

Inputs render `value`, `checked`, selection, disabled state, and visibility from
a View. Event handlers extract an immutable event payload and dispatch a
semantic intent. Application code must not later read the DOM to discover the
current value.

Native state such as caret position, IME composition internals, focus ring, and
scroll pixels may stay local until application behavior depends on it. At that
point it becomes a modeled interaction value.

### Frozen flamegraph

The widget Adapter receives a complete canonical projection:

```ts
interface ControlledFlamegraphAdapter {
  present(view: FlamegraphView, revision: number): void;
  onProposal(
    emit: (proposal: FlamegraphProposal, baseRevision: number) => void,
  ): () => void;
  dispose(): void;
}
```

The widget may mutate internally while handling a gesture. Its callback or
`getViewState()` is treated only as a proposal tagged with the model revision.
The authority accepts or rejects the proposal and the Adapter reapplies the
canonical projection. No other module may call `getViewState()`.

During migration, development builds should compare widget state with the
projection after every settled render and fail loudly on drift.

### URL and history

URL is a projection, not a store:

- initialization is one transaction: defaults, validated storage, then URL;
- URL wins over storage;
- hydration causes no echo write;
- trace-dependent anchors remain pending until the source is ready;
- invalid known values are dropped independently;
- unknown query pairs and foreign/future hashes are preserved;
- settled analytical changes use debounced `replaceState`;
- genuine navigation, including the existing API-mode scope/facet contract,
  uses `pushState`;
- `popstate` dispatches a route-observed event;
- Copy Link is an intent whose effect flushes URL projection before clipboard
  access.

The existing stable query/hash and legacy zoom rules remain unchanged.

### Resources and remote work

Create real seams only where an Adapter varies:

| Dependency | Category | Production and test Adapters |
| --- | --- | --- |
| Reducers, codecs, clamps, selectors | In-process | No port; keep inside the Module |
| History, storage, clocks, files, clipboard | Local-substitutable | Browser and memory/virtual-time Adapters |
| Frozen trace core and flamegraph | Local-substitutable | CJS/Worker/widget and fixture/headless Adapters |
| `/api/*` and SSE | Remote but owned | Fetch/SSE and scripted in-memory Adapters |
| Canvas and DOM rendering | Local-substitutable at tests | Browser renderer and recording/fake renderer where needed |

Do not create a port for every pure helper. That would move complexity into the
Interface without increasing Depth.

## Field policy

Every stored leaf must be exhaustively classified:

```ts
interface FieldPolicy {
  readonly role: "source" | "durable" | "preference" | "session" | "interaction" | "operation";
  readonly url: UrlPolicy;
  readonly storage: StoragePolicy;
  readonly resetOn: readonly ResetCause[];
  readonly sensitivity: "public" | "redacted" | "secret-ref";
  readonly lane: RenderLane | "none";
}

const viewerPolicy = {
  // ...
} satisfies StatePolicy<ViewerModel>;
```

`StatePolicy<T>` recursively requires every leaf in `T`. URL and storage
exclusions require a reason. This generalizes `VIEWER_STATE_OWNERSHIP` across
all pages and all meaningful state, rather than only current viewer store
fields.

The policy is not the URL codec. It is an exhaustive decision record from which
tests can enumerate durable fields and verify that codecs and reset behavior
exist.

## Preserving the property

Types alone cannot detect a new module variable. Add an architecture check
using the TypeScript compiler API, alongside `check-core-imports.mjs`.

The check should:

- reject imports of the writable runtime Implementation outside
  `src/state-authority/`;
- reject direct `history`, `location`, `localStorage`, `sessionStorage`,
  `fetch`, EventSource/SSE, timer, frozen-core, and flamegraph access outside
  their approved Adapters;
- reject `store.update` and generic patch helpers;
- reject direct widget `getViewState()` outside its Adapter;
- flag module-scope mutable declarations under `pages/` and `components/`;
- reject reads of element `.value`/`.checked` outside event translation or DOM
  Adapter code;
- require an explicit, reviewed suppression with a reason for mechanical
  resources and caches.

Also add these behavioral gates:

- reducer tests cover transitions through the public dispatch Seam;
- stale async completion tests exist for every operation family;
- URL contract tests enumerate all durable fields;
- Adapter reconstruction tests dispose and remount from one model snapshot;
- development transition journals record redacted intents, revisions, effects,
  and invalidated Views;
- development widget drift assertions compare frozen-widget materialization to
  canonical state;
- a PR checklist asks whether the change introduces rendered, behavioral,
  navigational, persisted, or async-lifecycle state.

The reconstruction test is the strongest whole-module test: after destroying
all render Adapters and attaching fresh ones, visible output and the result of
the next intent must be unchanged.

## Migration

Do not rewrite all five pages in one patch. Establish the rule horizontally,
then migrate complete page surfaces so each landed step has one authority.

### 0. Reframe the current PR

The deep-link work is useful and should not be discarded. Treat it as the
inventory and codec foundation, not proof of a unified store.

Before claiming unified state:

- classify meaningful local/DOM/widget state discovered in this audit;
- stop adding direct DOM reads or widget-to-store mirroring;
- keep URL behavior stable and byte-compatible;
- record this design as an ADR amendment.

If the PR needs to merge before the full migration, describe its guarantee
narrowly: "durable viewer state is projected to the URL." Do not describe the
whole UI as having one authority yet.

### 1. Add the State Authority Module

Add the pure program/reducer, effect result identity, typed Views, render lanes,
field policy, and architecture check. Keep the model private. A temporary
compatibility Adapter may expose old store reads, but generic writes must be
counted down and confined to that Adapter.

### 2. Trace viewer

Convert viewer `store.update` call groups into semantic intents. Move modal,
zoom-history, load lifecycle, copy feedback, and other meaningful closure state
into the model. Keep canvas handles and reconstructible caches local. Replace
subscriber-triggered synchronization with one root transition.

### 3. Browser

Make all form fields, checkboxes, raw selection, tabs, credentials status, and
request lifecycle controlled. URL serialization reads only the model. Secret
bytes move behind the credential Adapter.

### 4. Tokio Stats and Span Explorer

Give each page a complete page model. Move all module-level filters, tabs,
selection, sort, time range, stream/request lifecycle, and column overrides
behind dispatch. These pages are useful proof that the runtime is not
viewer-specific.

### 5. Flamegraph

Introduce the controlled widget Adapter. Migrate exact mode first while
preserving its legacy/hash dual write, then API and diff modes, including
facets, host sets, stream coverage, minimap band, split/collapse state, inspect
focus, and request identities. Remove the URL-mirror store and all application
reads of widget state.

### 6. Remove compatibility

Delete `update(slice, patch)`, the legacy compatibility Adapter, local-state
suppressions that are no longer needed, and drift assertions that have become
redundant. Keep architecture checks and reconstruction tests permanently.

## Acceptance criteria

The migration is complete when:

1. every page has exactly one `UiAuthority`;
2. no first-party module can mutate model fields directly;
3. every meaningful control and widget is controlled;
4. URL/storage are one-way projections plus observed navigation events;
5. every async result is identified and stale results cannot commit;
6. all model leaves have an exhaustive field policy;
7. the architecture checker reports no unexplained alternate authorities;
8. every page passes Adapter reconstruction and URL replay tests;
9. content and overlay performance budgets remain intact.

## Feedback on the idea

Yes: making one authority a design constraint is the right response to the
bugs. The old Redux architecture helped because it constrained writes and made
transitions inspectable.

No: centralizing every mutable implementation detail would create a giant,
high-churn model, make pointer/canvas work slower, expose credentials, and turn
resource handles into fake serializable state. "One object" is not the goal.
"One authority for meaningful behavior" is.

The most important change is not a larger `StoreState`. It is removing generic
patch writes and alternate authorities. A reducer with semantic intents,
controlled Adapters, and enforceable import/static rules will preserve that
property. Merely extending `VIEWER_STATE_OWNERSHIP` or bringing back a Redux
package will not.
