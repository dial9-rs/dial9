// Journey scripts J1-J9 — expert journeys made concrete as executable step
// lists (see lib/steps.mjs for the step vocabulary): J1 cold triage, J2
// worst-poll hunt, J3 locate a known moment, J4 follow a task, J5 flamegraph
// work, J6 S3 browse, J7 queue buildup, J8 share a view, J9 restore a
// recorded zoom link.
//
// `defaultPath` is the canonical target for each journey against a base URL
// (behavior-diff and perf-probe accept full page URLs, so different stacks or
// hosts can be compared). `checkpoint` steps mark where the behavioral
// differ captures the readout schema.

export const JOURNEYS = {
  J1: {
    label: "cold triage",
    page: "viewer",
    defaultPath: "/viewer.html?trace=demo-trace.bin",
    steps: [
      { checkpoint: "cold-open" },
      // The default layout keeps the analysis panels folded; triage starts
      // by opening them.
      { click: "#span-panel .chart-label" },
      { click: "#queue-chart .chart-label" },
      { sleep: 200 },
      { checkpoint: "panels-open" },
      { click: "#btn-next-poi" },
      { sleep: 200 },
      { checkpoint: "first-poi" },
    ],
  },

  J2: {
    label: "worst-poll hunt",
    page: "viewer",
    defaultPath: "/viewer.html?trace=demo-trace.bin",
    steps: [
      // "Worst first" is the default sort; step through the top three POIs.
      { click: "#btn-next-poi" },
      { click: "#btn-next-poi" },
      { click: "#btn-next-poi" },
      { sleep: 200 },
      { checkpoint: "third-poi" },
    ],
  },

  J3: {
    label: "locate a known moment",
    page: "viewer",
    defaultPath: "/viewer.html?trace=demo-trace.bin",
    steps: [
      // Absolute timestamps, then travel: arrow-key pans + zoom-at-cursor.
      { click: "#btn-time-mode" },
      { focus: "#main-area" },
      { key: ["ArrowRight", 3] },
      { wheel: { selector: "#lanes-container", dy: -240, modifier: "Control" } },
      { wheel: { selector: "#lanes-container", dy: -240, modifier: "Control" } },
      { sleep: 200 },
      { checkpoint: "zoomed-to-moment" },
    ],
  },

  J4: {
    label: "follow a task",
    page: "viewer",
    defaultPath: "/viewer.html?trace=demo-trace.bin",
    steps: [
      // A wake-delay POI selects its task (task-detail panel re-scopes).
      { click: "#btn-next-poi" },
      { sleep: 300 },
      { checkpoint: "task-selected" },
    ],
  },

  J5: {
    label: "flamegraph work",
    page: "flamegraph",
    defaultPath: "/flamegraph.html?trace=demo-trace.bin",
    steps: [
      { checkpoint: "rendered" },
      // "/" focuses frame search; Enter cycles matches.
      { key: "/" },
      { fill: [".fg-search-input", "poll"] },
      { press: [".fg-search-input", "Enter"] },
      { sleep: 200 },
      { checkpoint: "searched" },
    ],
  },

  J6: {
    label: "S3 browse",
    page: "index",
    fixedClock: true, // dev-seed date pinning — see lib/browser.mjs
    defaultPath: "/index.html",
    steps: [
      { click: ".quick-btns button:has-text('Last 24hr')" },
      { click: "#search-btn" },
      { waitFor: "#heatmap-labels .row" },
      { checkpoint: "heatmap" },
      { drag: { selector: "#heatmap-plot", from: [0.1, 0.5], to: [0.9, 0.5] } },
      { waitText: ["#selection-count", "segment"] },
      { checkpoint: "selected" },
    ],
  },

  J7: {
    label: "queue buildup",
    page: "viewer",
    defaultPath: "/viewer.html?trace=demo-trace.bin",
    steps: [
      { click: "#queue-chart .chart-label" },
      { sleep: 200 },
      { hoverSweep: ["#queue-canvas", 12] },
      { checkpoint: "queue-open" },
    ],
  },

  J8: {
    label: "share a view",
    page: "viewer",
    defaultPath: "/viewer.html?trace=demo-trace.bin",
    steps: [
      { checkpoint: "loaded" },
      // Zoom + pan, then look at the URL: this viewer never syncs the view
      // state to it, so url.query is the readout.
      { wheel: { selector: "#lanes-container", dy: -240, modifier: "Control" } },
      { focus: "#main-area" },
      { key: "ArrowRight" },
      { sleep: 200 },
      { checkpoint: "after-interaction" },
    ],
  },

  J9: {
    label: "restore a shared zoom link",
    page: "flamegraph",
    // A worker-zoom URL RECORDED from the flamegraph page itself: click-zoom
    // on the demo trace, copy the emitted URL, keep a child-by-child-walkable
    // prefix of its tab-joined path (the full emitted path is ~85 frames
    // deep; the prefix exercises the same restore code). Regenerate after a
    // demo-trace refresh by re-doing that click and re-recording. Restore
    // must NOT rewrite the URL, so the url.query readout doubles as the
    // no-write assertion; fg.breadcrumb is the zoom-visible readout.
    defaultPath:
      "/flamegraph.html?trace=demo-trace.bin&worker-zoom=" +
      "0xffff9b8cbf1c%090xffff9b862030%09Thread%3A%3Anew%3A%3Athread_start+unix.rs%3A130",
    steps: [
      { sleep: 200 },
      { checkpoint: "restored" },
    ],
  },
};
