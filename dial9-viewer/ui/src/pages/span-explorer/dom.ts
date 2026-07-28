// Element lookups for the Span Explorer shell. All behavior lives in the
// modules; the HTML carries only the static skeleton.

import { byId as lookupById } from "../../lib/dom/query.js";

const byId = <T extends HTMLElement>(id: string): T =>
  lookupById<T>("span-explorer shell", id);

/** The page's static elements, resolved once at boot. */
export interface PageEls {
  stats: HTMLElement;
  toolbar: HTMLElement;
  filterBar: HTMLElement;
  loading: HTMLElement;
  error: HTMLElement;
  catalogWrap: HTMLElement;
  catalog: HTMLTableElement;
  catalogBody: HTMLElement;
  detailPanel: HTMLElement;
  fStart: HTMLInputElement;
  fEnd: HTMLInputElement;
  btnApply: HTMLButtonElement;
  btnMore: HTMLButtonElement;
  btnStop: HTMLButtonElement;
  btnCopyLink: HTMLButtonElement;
}

/** Resolve the static shell elements; throws if the markup is incomplete. */
export function pageEls(): PageEls {
  return {
    stats: byId("stats"),
    toolbar: byId("toolbar"),
    filterBar: byId("filter-bar"),
    loading: byId("loading"),
    error: byId("error"),
    catalogWrap: byId("catalog-wrap"),
    catalog: byId<HTMLTableElement>("catalog"),
    catalogBody: byId("catalog-body"),
    detailPanel: byId("detail-panel"),
    fStart: byId<HTMLInputElement>("f-start"),
    fEnd: byId<HTMLInputElement>("f-end"),
    btnApply: byId<HTMLButtonElement>("btn-apply"),
    btnMore: byId<HTMLButtonElement>("btn-more"),
    btnStop: byId<HTMLButtonElement>("btn-stop"),
    btnCopyLink: byId<HTMLButtonElement>("btn-copylink"),
  };
}
