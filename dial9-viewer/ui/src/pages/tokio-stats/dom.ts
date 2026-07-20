// Element lookups for the tokio-stats shell. All behavior lives in the module.

/** The page's static elements, resolved once at boot. */
export interface PageEls {
  periods: HTMLElement;
  btnAdd: HTMLButtonElement;
  btnLoad: HTMLButtonElement;
  btnMore: HTMLButtonElement;
  slider: HTMLInputElement;
  threshLabel: HTMLElement;
  utcToggle: HTMLInputElement;
  status: HTMLElement;
  viewTabs: HTMLElement;
  summary: HTMLElement;
  tableContainer: HTMLElement;
}

import { byId as lookupById } from "../../lib/dom/query.js";

const byId = <T extends HTMLElement>(id: string): T =>
  lookupById<T>("tokio-stats shell", id);

/** Resolve the static shell elements; throws if the markup is incomplete. */
export function pageEls(): PageEls {
  return {
    periods: byId("periods"),
    btnAdd: byId("btn-add"),
    btnLoad: byId("btn-load"),
    btnMore: byId("btn-more"),
    slider: byId("threshold-slider"),
    threshLabel: byId("threshold-label"),
    utcToggle: byId("utc-toggle"),
    status: byId("status"),
    viewTabs: byId("view-tabs"),
    summary: byId("summary"),
    tableContainer: byId("table-container"),
  };
}
