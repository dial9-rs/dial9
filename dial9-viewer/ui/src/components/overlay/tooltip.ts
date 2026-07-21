// The shared hover tooltip COMPONENT (placement, render template,
// transient-channel updates). The hover DATA (LaneHoverData via
// assembleLaneHover) comes from elsewhere - this file never rescans the trace,
// it only renders what the assembler produced.
//
// AVOID MEASURE-AFTER-WRITE: a naive tooltip writes `tooltip.innerHTML` then
// IMMEDIATELY reads `tooltip.offsetWidth/offsetHeight` to place it, forcing a
// synchronous layout on every mousemove. Here:
//   - `placeTooltip` is a PURE function of the cursor, CACHED dimensions, and
//     the viewport - it never touches the DOM, so there is no measure-after-
//     write on the hover path.
//   - the cached dimensions are refreshed OUT OF BAND by a ResizeObserver
//     (which reports geometry with no forced synchronous layout), never by a
//     read that follows a write in the same frame.

import { html, render, type TemplateResult } from "lit-html";
import { formatHumanDuration } from "../../lib/trace/index.js";
import type { LaneHoverData } from "../canvas/lanes/index.js";

// Placement (pure, cached-dimension - avoids measure-after-write).

export interface TooltipDims {
  width: number;
  height: number;
}

export interface Viewport {
  width: number;
  height: number;
}

export interface CursorPos {
  clientX: number;
  clientY: number;
}

export interface TooltipPlacement {
  left: number;
  top: number;
}

/**
 * Position the tooltip near the cursor, clamped to the viewport on both axes,
 * flipping ABOVE the cursor when it would spill off the bottom. PURE: `dims`
 * are the CACHED tooltip dimensions (never read here) - this is the
 * read-after-write elimination. `dy` is the vertical offset below the cursor
 * (130 for the tall lane tooltip; 16 default for the rest).
 */
export function placeTooltip(
  cursor: CursorPos,
  dims: TooltipDims,
  viewport: Viewport,
  dy = 16,
): TooltipPlacement {
  const tx = Math.min(cursor.clientX + 12, viewport.width - dims.width - 8);
  let ty = cursor.clientY + dy;
  if (ty + dims.height > viewport.height - 8) ty = cursor.clientY - dims.height - 8;
  return { left: Math.max(8, tx), top: Math.max(8, ty) };
}

// Lane-hover content model (pure, testable).

/** One label/value(/hint) unit within a tooltip row. */
export interface TooltipSegment {
  label?: string;
  value?: string;
  /** Dim trailing note, e.g. "(click to view)". */
  hint?: string;
}

/** A tooltip line: one or more segments joined by " · " on render. */
export type TooltipRow = TooltipSegment[];

/** Formatting hooks the model needs (kept out so the model stays Node-pure). */
export interface TooltipFormatOpts {
  /** Format a trace-monotonic timestamp for the header (clock-mode aware). */
  formatTs(ns: number): string;
  /**
   * Windowed-data coverage at the cursor: a non-"complete" value adds an
   * explicit truncation warning row so a partial/oversized window is never
   * presented as whole data.
   */
  coverage?: "complete" | "truncated" | "oversized";
}

function onCpuIcon(ratio: number): string {
  return ratio >= 0.95 ? "🟢" : ratio >= 0.5 ? "🟡" : "🔴";
}

function schedDelayIcon(ns: number): string {
  return ns < 100_000 ? "🟢" : ns < 1_000_000 ? "🟡" : "🔴";
}

const STATE_LABEL: Record<LaneHoverData["state"], string> = {
  active: "🟢 Active",
  parked: "💤 Parked",
  block_in_place: "⚠️ block_in_place",
  polling: "⚡ Polling",
};

/**
 * Build the tooltip content rows from the assembled hover data. Pure over its
 * inputs (no DOM, no trace rescans) so the content mapping is testable.
 */
export function laneTooltipModel(
  data: LaneHoverData,
  opts: TooltipFormatOpts,
): TooltipRow[] {
  const rows: TooltipRow[] = [];

  // Header: Worker · Time.
  rows.push([
    { label: "Worker", value: String(data.workerId) },
    { label: "Time", value: opts.formatTs(data.ns) },
  ]);

  // State line, with the parked/block_in_place duration detail inline.
  let stateValue = STATE_LABEL[data.state];
  if (data.state === "parked" && data.parkDurationNs !== null) {
    stateValue += ` for ${formatHumanDuration(data.parkDurationNs)}`;
  } else if (data.state === "block_in_place" && data.blockInPlace !== null) {
    stateValue += ` for ${formatHumanDuration(data.blockInPlace.durationNs)}`;
  }
  rows.push([{ label: "State:", value: stateValue }]);

  // Scheduling / handoff (schedInfo): block_in_place handoff wins; else the
  // active on-CPU ratio (suppressed while parked).
  if (data.blockInPlace !== null) {
    rows.push([
      {
        label: "Worker handed off:",
        value: `tid ${data.blockInPlace.fromTid} → ${data.blockInPlace.toTid}`,
      },
    ]);
  } else if (data.state !== "parked" && data.onCpuRatio !== null) {
    const pct = (data.onCpuRatio * 100).toFixed(1);
    rows.push([
      { label: "Scheduling:", value: `${onCpuIcon(data.onCpuRatio)} ${pct}% on-CPU` },
    ]);
  }

  // Kernel sched delay (parked detail).
  if (data.kernelSchedDelayNs !== null) {
    rows.push([
      {
        label: "Kernel sched delay:",
        value: `${schedDelayIcon(data.kernelSchedDelayNs)} ${formatHumanDuration(
          data.kernelSchedDelayNs,
        )}`,
      },
    ]);
  }

  // Poll detail (state polling).
  const poll = data.poll;
  if (poll !== null) {
    const openTag = poll.openEnded ? " (open-ended, block_in_place)" : "";
    rows.push([
      { label: "Poll duration:", value: `${formatHumanDuration(poll.durationNs)}${openTag}` },
    ]);
    if (poll.taskId !== null) {
      rows.push([{ label: "Task ID:", value: `0x${poll.taskId.toString(16)}` }]);
      if (poll.spawnLoc !== null) {
        rows.push([{ label: "Spawned at:", value: poll.spawnLoc }]);
      }
    }
    if (poll.cpuSampleCount > 0) {
      rows.push([
        { label: "CPU samples:", value: `🔥 ${poll.cpuSampleCount}`, hint: "(click to view)" },
      ]);
    }
    if (poll.schedSampleCount > 0) {
      rows.push([
        {
          label: "Sched events:",
          value: `⚠️ ${poll.schedSampleCount} blocking`,
          hint: "(click to view)",
        },
      ]);
    }
    if (poll.spanCount > 0) {
      const summary = Object.entries(poll.spanNames)
        .map(([n, c]) => `${n}×${c}`)
        .join(", ");
      rows.push([{ label: "Spans:", value: String(poll.spanCount), hint: `(${summary})` }]);
    }
  }

  // Queue depths ("-" for missing).
  const g = data.globalQueue;
  const l = data.localQueue;
  const total = (g ?? 0) + (l ?? 0);
  rows.push([
    { label: "Global Q:", value: g !== null ? String(g) : "-" },
    { label: "Local Q:", value: l !== null ? String(l) : "-" },
    { label: "Est. Total:", value: String(total) },
  ]);

  // Active-task count (only when the trace tracks tasks).
  if (data.activeTaskCount !== null) {
    rows.push([{ label: "Active Tasks:", value: String(data.activeTaskCount) }]);
  }

  // Span detail (name + duration, fields, parent).
  const span = data.span;
  if (span !== null) {
    rows.push([{ label: "Span:", value: `${span.name} ${formatHumanDuration(span.durationNs)}` }]);
    if (span.fields.length > 0) {
      const fieldStr = span.fields.map(([k, v]) => `${k}=${String(v)}`).join(", ");
      rows.push([{ label: "Fields:", value: fieldStr }]);
    }
    if (span.parentName !== null) {
      rows.push([{ label: "Parent:", value: span.parentName }]);
    }
  }

  // Surface a partial/oversized window explicitly.
  if (opts.coverage === "truncated") {
    rows.push([{ label: "Window:", value: "⚠️ partial data (window edge - not the full extent)" }]);
  } else if (opts.coverage === "oversized") {
    rows.push([{ label: "Window:", value: "⚠️ oversized segment - tier-1 view only (not the full extent)" }]);
  }

  return rows;
}

// lit-html render (declarative; no measure-after-write).

/** Build the full lane-hover tooltip template from assembled hover data. */
export function buildLaneTooltip(
  data: LaneHoverData,
  opts: TooltipFormatOpts,
): TemplateResult {
  return tooltipRowsTemplate(laneTooltipModel(data, opts));
}

/** Render tooltip rows as an escaped lit-html template (auto-escapes). */
export function tooltipRowsTemplate(rows: readonly TooltipRow[]): TemplateResult {
  return html`${rows.map(
    (row, i) => html`${i > 0 ? html`<br />` : ""}${row.map(
        (seg, j) =>
          html`${j > 0 ? html` · ` : ""}${seg.label
            ? html`<span class="label">${seg.label}</span> `
            : ""}${seg.value !== undefined
            ? html`<span class="value">${seg.value}</span>`
            : ""}${seg.hint ? html` <span class="hint">${seg.hint}</span>` : ""}`,
      )}`,
  )}`;
}

// Stateful element (dimension cache; the ResizeObserver guard).

export interface TooltipHandle {
  /** Render `content` into the tooltip, show it, and place it at `cursor`. */
  show(content: TemplateResult, cursor: CursorPos, dy?: number): void;
  /** Hide the tooltip (hide-on-drag/leave). */
  hide(): void;
  /** The last cached dimensions (for tests/diagnostics). */
  dims(): TooltipDims;
  /** Tear down (removes the element + observer). */
  dispose(): void;
}

/** The viewport size, read once per placement from the window (no element read). */
function windowViewport(): Viewport {
  return {
    width: typeof window !== "undefined" ? window.innerWidth : 0,
    height: typeof window !== "undefined" ? window.innerHeight : 0,
  };
}

/**
 * Create the single shared `#tooltip` element (one element, all panels reuse
 * it). Placement reads CACHED dims kept fresh by a ResizeObserver, so no
 * hover-path code ever measures the element after writing it. The observer
 * geometry is delivered post-layout with no forced synchronous reflow.
 */
export function createTooltip(doc: Document = document): TooltipHandle {
  const el = doc.createElement("div");
  el.className = "d9-tooltip";
  el.setAttribute("role", "tooltip");
  el.style.display = "none";
  doc.body.appendChild(el);

  // Cached dimensions: seeded from the CSS max-width + a small default so the
  // very first placement is sane, then kept exact by the observer.
  let cached: TooltipDims = { width: 320, height: 24 };

  let observer: ResizeObserver | null = null;
  if (typeof ResizeObserver !== "undefined") {
    observer = new ResizeObserver((entries) => {
      const entry = entries[0];
      if (!entry) return;
      const box = entry.borderBoxSize && entry.borderBoxSize[0];
      if (box) {
        cached = { width: box.inlineSize, height: box.blockSize };
      } else {
        // Fallback path (older ResizeObserver): contentRect is post-layout,
        // still not a forced synchronous read.
        cached = { width: entry.contentRect.width, height: entry.contentRect.height };
      }
    });
    observer.observe(el);
  }

  return {
    show(content, cursor, dy = 16) {
      render(content, el);
      el.style.display = "block";
      const p = placeTooltip(cursor, cached, windowViewport(), dy);
      el.style.left = `${p.left}px`;
      el.style.top = `${p.top}px`;
    },
    hide() {
      el.style.display = "none";
    },
    dims: () => cached,
    dispose() {
      observer?.disconnect();
      el.remove();
    },
  };
}
