# PROTOTYPE: unified UI state authority

This is throwaway code. Do not import it from the viewer.

## Question

Can one pure, page-agnostic transition Interface own all meaningful state
across the browser, viewer, flamegraph, Tokio Stats, and Span Explorer pages
without forcing DOM handles, canvas resources, credentials, or caches into the
model?

The prototype makes the proposed model visible after every action. It exercises:

- controlled form and selection intents on all five pages;
- URL hydration and projection with unknown query fields preserved;
- atomic cross-field transitions;
- identified asynchronous loads and stale-result rejection;
- separate chrome, content, and overlay invalidation lanes;
- a frozen flamegraph treated as a controlled projection;
- session-only state that is intentionally absent from the URL.
- exhaustive top-level field policy: a new page-model field fails TypeScript
  until classified (the production version should recurse through every leaf).

It does not implement real DOM, canvas, URL, HTTP, or widget Adapters. Effects
are printed so their ordering and payloads can be inspected.

## Run

From `dial9-viewer/ui`:

```bash
npm run prototype:state-authority
```

For a non-interactive walkthrough of every page:

```bash
npm run prototype:state-authority -- --demo
```

The reusable part under evaluation is the pure `initialize`/`evolve` Interface
in `model.mts`. The terminal shell in `cli.mts` is disposable.
