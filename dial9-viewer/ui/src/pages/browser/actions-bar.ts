// Actions bar: View Selected / Flamegraph / Tokio Stats buttons, the
// selection count and the size-cap warning, rendered from store state.
//
// When a full A/B diff is captured, the Flamegraph and Tokio Stats buttons
// open the two-sided diff instead of a single-scope view (see
// open-links.ts), so their labels say so.

// Leaf heatmap seam, not the lib/canvas barrel (see actions.ts).
import { MAX_OPEN_BYTES } from "../../lib/canvas/heatmap.js";
import { assertInScheduledRender } from "../../store/store.js";
import type { PageCtx } from "./ctx.js";
import { fmtTick, formatSize } from "./format.js";
import { effectiveProfileSelection } from "./open-links.js";

interface ButtonLabel {
  text: string;
  title: string;
}

/**
 * Labels for the two profiling buttons. `diffMode` is "both diff sides are
 * captured" - the state in which these buttons retarget to the two-sided
 * diff. A partial capture still opens the single-scope view, so it keeps the
 * plain labels.
 */
export function profileButtonLabels(diffMode: boolean): {
  flamegraph: ButtonLabel;
  tokio: ButtonLabel;
} {
  if (diffMode) {
    return {
      flamegraph: {
        text: "🔥 Flamegraph diff",
        title: "Open a two-sided CPU flamegraph diff of the captured A/B scopes",
      },
      tokio: {
        text: "⚡ Tokio Stats diff",
        title: "Open a two-sided Tokio stats diff of the captured A/B scopes",
      },
    };
  }
  return {
    flamegraph: {
      text: "🔥 Flamegraph",
      title: "Open a CPU flamegraph of the selected segments",
    },
    tokio: {
      text: "⚡ Tokio Stats",
      title: "Open Tokio stats view for the selected scope",
    },
  };
}

export function mountActionsBar({ store, els, actions }: PageCtx): void {
  els.viewBtn.addEventListener("click", () => {
    actions.viewSelected();
  });
  els.cpuBtn.addEventListener("click", () => {
    actions.viewCpuProfile();
  });
  els.healthBtn.addEventListener("click", () => {
    actions.viewTokioStats();
  });
  els.spansBtn.addEventListener("click", () => {
    actions.viewSpanExplorer();
  });

  store.subscribe(["browse", "raw", "ui", "config", "diff"], (state) => {
    assertInScheduledRender("actions-bar render");
    const agg = state.config.aggregationEnabled;

    // Relabel BEFORE the per-tab branches below: they return early on an
    // empty selection, and the label tracks the captured diff, not the
    // selection.
    const labels = profileButtonLabels(!!(state.diff.a && state.diff.b));
    els.cpuBtn.textContent = labels.flamegraph.text;
    els.cpuBtn.title = labels.flamegraph.title;
    els.healthBtn.textContent = labels.tokio.text;
    els.healthBtn.title = labels.tokio.title;

    if (state.ui.tab === "browse") {
      const sel = state.browse.selection;
      const profileSel = effectiveProfileSelection(state.browse);
      if (!profileSel) {
        els.viewBtn.disabled = true;
        els.cpuBtn.disabled = true;
        els.healthBtn.disabled = true;
        els.spansBtn.disabled = true;
        els.selectionWarn.textContent = "";
        els.selectionCount.textContent = "";
        return;
      }
      // Selection size cap (MAX_OPEN_BYTES). Flamegraph is exempt in
      // aggregation mode (the server samples; no client decode) and Tokio
      // Stats is never capped.
      const over = profileSel.bytes > MAX_OPEN_BYTES;
      els.viewBtn.disabled = !sel || over;
      els.cpuBtn.disabled = over && !agg;
      els.healthBtn.disabled = !agg;
      // Span Explorer is aggregate-only: /api/span-stats is the only source of
      // span statistics, and it never client-decodes.
      els.spansBtn.disabled = !agg;
      els.selectionWarn.textContent =
        over && !agg
          ? `Too large to open (${formatSize(profileSel.bytes)} > ${formatSize(MAX_OPEN_BYTES)}) — narrow your selection.`
          : "";
      const tz = state.ui.useLocalTz;
      const win =
        profileSel.t0 && profileSel.t1
          ? ` · ${fmtTick(profileSel.t0, tz)}–${fmtTick(profileSel.t1, tz)}`
          : "";
      const scopeLabel = sel ? "" : "Current service · ";
      els.selectionCount.textContent = `${scopeLabel}${profileSel.keys.length} segment${profileSel.keys.length !== 1 ? "s" : ""} · ${formatSize(profileSel.bytes)}${win}`;
      return;
    }

    // Raw mode. The (hidden) Flamegraph / Tokio Stats / Spans buttons are
    // browse-selection territory; leave their disabled state untouched.
    const count = state.raw.selected.size;
    els.selectionWarn.textContent = "";
    els.selectionCount.textContent = count ? `${count} selected` : "";
    els.viewBtn.disabled = count === 0;
  });
}
