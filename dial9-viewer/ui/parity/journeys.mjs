// Journey scripts J1-J8 — the eight expert journeys from
// docs/ui-inventory/04-ux-findings.md ("Method": J1 cold triage, J2
// worst-poll hunt, J3 locate a known moment, J4 follow a task, J5 flamegraph
// work, J6 S3 browse, J7 queue buildup, J8 share a view), made concrete as
// executable step lists (see lib/steps.mjs for the step vocabulary).
//
// The audit-phase scripts these derive from lived in an ephemeral session
// scratchpad and are gone; these step lists are authored fresh against the
// journey labels and drive the same flows against the demo trace / dev seed.
//
// `defaultPath` is the canonical target for each journey against a base URL
// (behavior-diff and perf-probe accept full page URLs, so old/new stacks or
// different hosts can be compared). `checkpoint` steps mark where the
// behavioral differ captures the readout schema.

export const JOURNEYS = {
  J1: {
    label: "cold triage",
    page: "viewer",
    defaultPath: "/viewer.html?trace=demo-trace.bin",
    steps: [
      { checkpoint: "cold-open" },
      // The default layout keeps the analysis panels folded (finding S1);
      // triage starts by opening them.
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
      // Zoom + pan, then look at the URL: on the legacy viewer the view
      // state never reaches it (finding S3) — url.query is the readout.
      { wheel: { selector: "#lanes-container", dy: -240, modifier: "Control" } },
      { focus: "#main-area" },
      { key: "ArrowRight" },
      { sleep: 200 },
      { checkpoint: "after-interaction" },
    ],
  },
};
