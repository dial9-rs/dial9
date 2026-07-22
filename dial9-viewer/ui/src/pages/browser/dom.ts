// Typed element handles for the browser page. The ids/classes match the
// page markup and are load-bearing for the row walkers.

import {
  byId as lookupById,
  byIdOf as lookupByIdOf,
  bySelector as lookupBySelector,
} from "../../lib/dom/query.js";

const SHELL = "browser page";
const byId = <T extends HTMLElement>(id: string): T => lookupById<T>(SHELL, id);
const byIdOf = <T extends HTMLElement>(id: string, ctor: new () => T): T =>
  lookupByIdOf(SHELL, id, ctor);
const bySelector = <T extends HTMLElement>(sel: string): T =>
  lookupBySelector<T>(SHELL, sel);

export interface BrowserEls {
  // Header
  tzBtn: HTMLButtonElement;
  credsBtn: HTMLButtonElement;
  credsBtnLabel: HTMLSpanElement;
  // Credentials panel
  credsPanel: HTMLDivElement;
  credsClose: HTMLButtonElement;
  credsPaste: HTMLTextAreaElement;
  credsPasteFill: HTMLButtonElement;
  credsAkid: HTMLInputElement;
  credsSecret: HTMLInputElement;
  credsToken: HTMLInputElement;
  credsRegion: HTMLInputElement;
  credsApply: HTMLButtonElement;
  credsClear: HTMLButtonElement;
  credsStatus: HTMLSpanElement;
  credsBucketsRow: HTMLDivElement;
  credsBuckets: HTMLDivElement;
  // Controls bar
  bucketInput: HTMLInputElement;
  prefixInput: HTMLInputElement;
  prefixSuggestions: HTMLSpanElement;
  quickBtns: readonly HTMLButtonElement[];
  rangeFrom: HTMLInputElement;
  rangeTo: HTMLInputElement;
  searchBtn: HTMLButtonElement;
  // Tabs + views
  tabBrowse: HTMLButtonElement;
  tabRaw: HTMLButtonElement;
  browseView: HTMLDivElement;
  rawView: HTMLDivElement;
  browseWarning: HTMLDivElement;
  browseStatus: HTMLDivElement;
  // Heatmap
  heatmapView: HTMLDivElement;
  heatmapLabels: HTMLDivElement;
  heatmapPlot: HTMLDivElement;
  heatmapCanvas: HTMLCanvasElement;
  heatmapSel: HTMLDivElement;
  heatmapAxis: HTMLDivElement;
  heatmapResetZoom: HTMLButtonElement;
  // Raw search
  rawSearchInput: HTMLInputElement;
  rawSearchBtn: HTMLButtonElement;
  rawStatus: HTMLDivElement;
  rawTable: HTMLTableElement;
  rawBody: HTMLTableSectionElement;
  rawSelectAll: HTMLInputElement;
  // Actions bar
  actionsBar: HTMLDivElement;
  rawActions: HTMLSpanElement;
  rawSelectAllBtn: HTMLButtonElement;
  rawDeselectAllBtn: HTMLButtonElement;
  viewBtn: HTMLButtonElement;
  cpuBtn: HTMLButtonElement;
  healthBtn: HTMLButtonElement;
  selectionWarn: HTMLSpanElement;
  selectionCount: HTMLSpanElement;
  // Footer
  footerDrop: HTMLElement;
  loadDemoBtn: HTMLButtonElement;
}

export function queryEls(): BrowserEls {
  return {
    tzBtn: byId("tz-btn"),
    credsBtn: byId("creds-btn"),
    credsBtnLabel: byId("creds-btn-label"),
    credsPanel: byId("creds-panel"),
    credsClose: byId("creds-close"),
    credsPaste: byId("creds-paste"),
    credsPasteFill: byId("creds-paste-fill"),
    credsAkid: byId("creds-akid"),
    credsSecret: byId("creds-secret"),
    credsToken: byId("creds-token"),
    credsRegion: byId("creds-region"),
    credsApply: byId("creds-apply"),
    credsClear: byId("creds-clear"),
    credsStatus: byId("creds-status"),
    credsBucketsRow: byId("creds-buckets-row"),
    credsBuckets: byId("creds-buckets"),
    bucketInput: byId("bucket-input"),
    prefixInput: byId("prefix-input"),
    prefixSuggestions: byId("prefix-suggestions"),
    quickBtns: [...document.querySelectorAll<HTMLButtonElement>(".quick-btns button")],
    rangeFrom: byId("range-from"),
    rangeTo: byId("range-to"),
    searchBtn: byId("search-btn"),
    tabBrowse: byId("tab-browse"),
    tabRaw: byId("tab-raw"),
    browseView: byId("browse-view"),
    rawView: byId("raw-view"),
    browseWarning: byId("browse-warning"),
    browseStatus: byId("browse-status"),
    heatmapView: byId("heatmap-view"),
    heatmapLabels: byId("heatmap-labels"),
    heatmapPlot: byId("heatmap-plot"),
    heatmapCanvas: byIdOf("heatmap-canvas", HTMLCanvasElement),
    heatmapSel: byId("heatmap-sel"),
    heatmapAxis: byId("heatmap-axis"),
    heatmapResetZoom: byId("heatmap-reset-zoom"),
    rawSearchInput: byId("raw-search-input"),
    rawSearchBtn: bySelector('button[data-action="raw-search"]'),
    rawStatus: byId("raw-status"),
    rawTable: byId("raw-table"),
    rawBody: byId("raw-body"),
    rawSelectAll: byId("raw-select-all"),
    actionsBar: byId("actions-bar"),
    rawActions: byId("raw-actions"),
    rawSelectAllBtn: bySelector('button[data-action="raw-select-all"]'),
    rawDeselectAllBtn: bySelector('button[data-action="raw-deselect-all"]'),
    viewBtn: byId("view-btn"),
    cpuBtn: byId("cpu-btn"),
    healthBtn: byId("health-btn"),
    selectionWarn: byId("selection-warn"),
    selectionCount: byId("selection-count"),
    footerDrop: byId("footer-drop"),
    loadDemoBtn: byId("load-demo-btn"),
  };
}
