// Hand-written type declarations for the frozen-core file `panel_layout.js`
// (shared time-panel layout math). See src/types/decode.d.ts for the
// declaration-form rationale. Verified against panel_layout.js and the
// viewer.html call site (timePanelLayout wrapper, which passes an
// optionally-undefined scrollbarW).

declare module "*/panel_layout.js" {
  export interface TimePanelLayout {
    /** Full panel/canvas width in CSS px. */
    pw: number;
    /** Left gutter width reserved for labels. */
    labelW: number;
    /** pw - labelW - scrollbarW. */
    drawW: number;
    /** Timestamp (ns) -> canvas x (labelW-shifted). */
    nsToPanelX(ns: number): number;
    /** Like nsToPanelX, clamped to [labelW, labelW + drawW]. */
    nsToPanelXClamped(ns: number): number;
    /** Canvas x -> timestamp (ns). */
    panelXToNs(x: number): number;
  }

  /**
   * Build a time-panel layout view. Pure function -- no globals, no DOM.
   * `scrollbarW` may be undefined (treated as 0), matching the viewer.html
   * wrapper which forwards an optional parameter positionally.
   */
  export function makeTimePanelLayout(
    pw: number,
    labelW: number,
    scrollbarW: number | undefined,
    viewStart: number,
    viewEnd: number
  ): TimePanelLayout;
}
