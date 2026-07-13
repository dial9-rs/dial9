// src/pages/tokio-stats/dom.ts - element lookups for the migrated tokio-stats
// shell (new/tokio_stats.html). The static markup is byte-faithful to the
// legacy page so the affordance census is zero-diff (the switch control is
// the only intentional addition); ALL behavior lives in the module.

/** The page's static elements, resolved once at boot. */
export interface PageEls {
  periods: HTMLElement;
  btnAdd: HTMLButtonElement;
  btnLoad: HTMLButtonElement;
  slider: HTMLInputElement;
  threshLabel: HTMLElement;
  utcToggle: HTMLInputElement;
  status: HTMLElement;
  viewTabs: HTMLElement;
  summary: HTMLElement;
  tableContainer: HTMLElement;
}

function byId<T extends HTMLElement>(id: string): T {
  const el = document.getElementById(id);
  if (el === null) throw new Error(`tokio-stats shell is missing #${id}`);
  return el as T;
}

/** Resolve the static shell elements; throws if the markup is incomplete. */
export function pageEls(): PageEls {
  return {
    periods: byId("periods"),
    btnAdd: byId("btn-add"),
    btnLoad: byId("btn-load"),
    slider: byId("threshold-slider"),
    threshLabel: byId("threshold-label"),
    utcToggle: byId("utc-toggle"),
    status: byId("status"),
    viewTabs: byId("view-tabs"),
    summary: byId("summary"),
    tableContainer: byId("table-container"),
  };
}
