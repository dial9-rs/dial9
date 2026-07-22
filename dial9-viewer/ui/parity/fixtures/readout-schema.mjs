// The readout schema — the fixture, owned by the parity tool, that defines
// which data readouts the behavioral differ compares field-by-field.
//
// Field kinds:
//   text     — trimmed textContent of `selector`; if `regex` is set, capture
//              group 1 of the first match (null when no match);
//   value    — input/select value;
//   count    — number of elements matching `selector`;
//   enabled  — whether the element is enabled (buttons);
//   urlQuery — the page URL's query string + hash (origin-independent, so
//              two hosts serving the same state compare equal).
//
// `volatile: true` — captured and reported for context, EXCLUDED from the
// diff (wall-clock dependent, e.g. load time).
// `optional: true` — element may legitimately be absent/hidden; reads as
// null instead of an error.

export const READOUT_SCHEMA = {
  // viewer.html — the trace viewer
  viewer: [
    { id: "events.count", kind: "text", selector: "#toolbar-row-data", regex: "([\\d,]+) events" },
    { id: "workers.count", kind: "text", selector: "#toolbar-row-data", regex: "(\\d+) workers" },
    { id: "trace.duration", kind: "text", selector: "#toolbar-row-data", regex: "workers · ([^·\\s]+)" },
    { id: "load.time", kind: "text", selector: ".load-time-suffix", regex: "loaded in (.+)", optional: true, volatile: true },
    { id: "poi.counter", kind: "text", selector: "#poi-counter" },
    { id: "poi.filterValue", kind: "value", selector: "#poi-filter" },
    { id: "poi.sortByWorst", kind: "value", selector: "#sort-by-worst" },
    { id: "flamegraph.buttonLabel", kind: "text", selector: "#btn-cpu-flamegraph", optional: true },
    { id: "blindSpawns.buttonLabel", kind: "text", selector: "#btn-uninstrumented-info", optional: true },
    { id: "lanes.count", kind: "count", selector: "#lanes-container .lane" },
    { id: "spans.panelInfo", kind: "text", selector: "#span-panel-info", optional: true },
    { id: "spans.legendChips", kind: "count", selector: "#span-legend-items .span-chip" },
    { id: "events.panelInfo", kind: "text", selector: "#ce-panel-info", optional: true },
    { id: "cpu.panelInfo", kind: "text", selector: "#cpu-panel-info", optional: true },
    { id: "queue.infoPanel", kind: "text", selector: "#info-panel", optional: true },
    { id: "taskDetail.status", kind: "text", selector: "#task-detail-status", optional: true },
    { id: "sidebar.title", kind: "text", selector: "#stack-sidebar-title", optional: true },
    { id: "url.query", kind: "urlQuery" },
  ],

  // index.html — the S3 trace browser
  index: [
    { id: "selection.count", kind: "text", selector: "#selection-count" },
    { id: "selection.warn", kind: "text", selector: "#selection-warn" },
    { id: "browse.status", kind: "text", selector: "#browse-status", optional: true },
    { id: "heatmap.hostRows", kind: "count", selector: "#heatmap-labels .row" },
    { id: "heatmap.axisTicks", kind: "count", selector: "#heatmap-axis .tick" },
    { id: "raw.rows", kind: "count", selector: "#raw-body tr" },
    { id: "actions.viewEnabled", kind: "enabled", selector: "#view-btn" },
    { id: "actions.flamegraphEnabled", kind: "enabled", selector: "#cpu-btn" },
    { id: "url.query", kind: "urlQuery" },
  ],

  // flamegraph.html — the standalone CPU flamegraph
  flamegraph: [
    { id: "fg.title", kind: "text", selector: "#fg-title" },
    { id: "fg.stats", kind: "text", selector: "#fg-stats" },
    { id: "fg.searchStats", kind: "text", selector: ".fg-search-stats", optional: true },
    { id: "fg.canvases", kind: "count", selector: ".fg-canvas" },
    // Zoom-visible readout: the breadcrumb trail renders the active zoom
    // path; empty when not zoomed.
    { id: "fg.breadcrumb", kind: "text", selector: ".fg-breadcrumb", optional: true },
    { id: "url.query", kind: "urlQuery" },
  ],
};
