// Pluggable render-invocation sources for the perf probe.
//
// The probe reports "render invocations per frame" — how many times the UI's
// render layer ran per painted frame during the interaction storm. WHERE that
// counter comes from is stack-specific, so it is a pluggable source chosen
// with --render-source:
//
//   install(page)  awaited BEFORE page.goto — e.g. addInitScript so counters
//                  exist from the first app script;
//   reset(page)    awaited at storm start — zero the counters so the numbers
//                  cover the storm window only;
//   collect(page)  awaited at storm end — returns
//                    { available: boolean, note?: string, counters?: object }
//                  `counters` values are invocation counts for the storm
//                  window; the probe divides them by painted frames.
//
// Sources:
//   stub (default) — placeholder for any page that exposes no render-counter
//     hook. Always reports available:false.

export const RENDER_SOURCES = {
  stub: {
    description:
      "placeholder source: reports render counters as unavailable",
    async install() {},
    async reset() {},
    async collect() {
      return {
        available: false,
        note:
          "stub render source — render-invocation counters unavailable on this page. " +
          "Chunk 2 adds the `store` source backed by the store scheduler's devRenderAssertStats() dev hook.",
      };
    },
  },
};
