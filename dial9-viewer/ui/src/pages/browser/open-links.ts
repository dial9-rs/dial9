// The deep links this page opens: the viewer, flamegraph and tokio-stats
// URLs built from the current selection. Every link is self-contained (the
// bucket's region stamped on, the selection encoded as a scope) so the
// opened tab re-resolves the files by itself rather than inheriting any of
// this page's state.

// Leaf seam modules, NOT the lib barrels: the barrel indexes evaluate
// modules that import trace_analysis.js / trace_parser.js at init (they
// expect <script>-established globals), which this page must not load.
import { traceTitleParams } from "../../lib/trace/title.js";
import {
  encodeAggregationParams,
  encodeScope,
  scopeFromKeys,
} from "../../lib/trace/trace_scope.js";
import type { BrowserEls } from "./dom.js";
import type { BrowserStore } from "./state.js";

// The bucket's region, stamped onto every link this page opens so the
// opened tab's URL is self-contained (signs the right regional S3 endpoint
// without inheriting a detected region). Source of truth is the credentials
// store; the panel's region field is the fallback. "" when unknown (the
// server then uses its default region).
function currentRegion(els: BrowserEls): string {
  const stored = window.Dial9Creds ? window.Dial9Creds.get() : null;
  if (stored && stored.region) return stored.region;
  return els.credsRegion.value.trim() || "";
}

// The selection spanned too many hosts to name them all in the URL, so the
// scope degraded to "all hosts in the time window". Warn that the opened
// view may be broader than the literal box selection.
function warnHostsDropped(): void {
  alert(
    "This selection spans too many hosts to put in the link, so the opened " +
      "view covers ALL hosts in the selected time window. Narrow the time " +
      "range or host selection for an exact match.",
  );
}

// The current selection as an aggregate scope (bucket/prefix/service/
// host-set + start_ns/end_ns), shared with the flamegraph/tokio buttons so
// all three agree on how a box maps to a scope. Null when there's no usable
// selection (or no bucket).
export function selectionScope(
  store: BrowserStore,
  els: BrowserEls,
): URLSearchParams | null {
  const sel = store.getState().browse.selection;
  if (!sel || !sel.keys.length) return null;
  const bucket = els.bucketInput.value.trim();
  if (!bucket) return null;
  const scope = scopeFromKeys(bucket, sel.keys, sel.t0, sel.t1, currentRegion(els));
  if (!scope) return null;
  const { query, hostsDropped } = encodeAggregationParams(new URLSearchParams(), scope);
  if (hostsDropped) warnHostsDropped();
  return new URLSearchParams(query);
}

export interface OpenLinksDeps {
  store: BrowserStore;
  els: BrowserEls;
  /** Selection keys in the active tab's order (browse box or raw table). */
  getSelectedKeys(): string[];
  /** A captured A/B diff takes over the flamegraph/tokio buttons (#623). */
  launchDiff(kind: "flamegraph" | "tokio"): void;
}

export interface OpenLinks {
  viewSelected(): void;
  viewCpuProfile(): void;
  viewTokioStats(): void;
  viewSpanExplorer(): void;
}

export function createOpenLinks(deps: OpenLinksDeps): OpenLinks {
  const { store, els, getSelectedKeys, launchDiff } = deps;

  function localTz(): boolean {
    return store.getState().ui.useLocalTz;
  }

  // Open the viewer on the selection. Carry it as a compact *scope*
  // (bucket/prefix/service/host-set + time window) rather than one trace=
  // component per file, so a large selection's URL stays under CloudFront's
  // 8192-byte cap and re-resolves in any browser (#589). Local mode is the
  // exception: buffer-style local key names carry nothing to derive a scope
  // from, so open those directly by key (#627).
  function viewSelected(): void {
    const keys = getSelectedKeys();
    if (keys.length === 0) return;

    const bucket = els.bucketInput.value.trim();

    if (store.getState().config.localMode) {
      const params = new URLSearchParams();
      for (const key of keys) {
        params.append("trace", "/api/object?key=" + encodeURIComponent(key));
      }
      window.open("viewer.html?" + params.toString(), "_blank");
      return;
    }

    // Structured metadata for the viewer title, plus the scope the viewer
    // re-lists files from. Unknown-layout keys don't leak shifted svc/host.
    const titleParams = traceTitleParams(keys, { localTz: localTz() });
    const sel = store.getState().browse.selection;
    const scope = scopeFromKeys(
      bucket,
      keys,
      sel ? sel.t0 : null,
      sel ? sel.t1 : null,
      currentRegion(els),
    );
    // Raw mode passes no window, so scopeFromKeys derives it from the keys'
    // epochs; null means an unrecognized layout with nothing to scope.
    if (!scope) {
      alert(
        "Could not derive a time range from the selected files. Their key " +
          "names may use an unrecognized layout.",
      );
      return;
    }
    const { query, hostsDropped } = encodeScope(titleParams, scope);
    if (hostsDropped) warnHostsDropped();
    window.open(`viewer.html?${query}`, "_blank");
  }

  // Flamegraph button. Two modes: aggregation-enabled servers get the sampled
  // server-side refinement loop over the selection's scope (encoded in the
  // un-namespaced aggregation vocabulary, `api=1` selecting the demand-driven
  // path); otherwise the exact per-file decode over a compact `s_*` scope.
  // A captured A/B diff routes the button to the two-sided diff instead (#623).
  function viewCpuProfile(): void {
    const s = store.getState();
    if (s.diff.a && s.diff.b) {
      launchDiff("flamegraph");
      return;
    }
    const sel = s.browse.selection;
    if (!sel || !sel.keys.length) return;
    const bucket = els.bucketInput.value.trim();
    if (!bucket) {
      alert("Bucket is required");
      return;
    }
    const scope = scopeFromKeys(bucket, sel.keys, sel.t0, sel.t1, currentRegion(els));
    if (!scope) return; // a box selection always carries a window, so unreachable

    if (s.config.aggregationEnabled) {
      const base = new URLSearchParams();
      base.set("api", "1");
      const { query, hostsDropped } = encodeAggregationParams(base, scope);
      if (hostsDropped) warnHostsDropped();
      window.open("flamegraph.html?" + query, "_blank");
      return;
    }

    // Exact mode: open the selected raw traces and decode client-side.
    const fgParams = traceTitleParams(sel.keys, { localTz: localTz() });
    const { query, hostsDropped } = encodeScope(fgParams, scope);
    if (hostsDropped) warnHostsDropped();
    window.open("flamegraph.html?" + query, "_blank");
  }

  // Tokio Stats button: the same aggregation scope the demand-driven
  // flamegraph uses (/api/tokio-stats reads the same param vocabulary). A
  // captured A/B diff routes to the two-sided diff instead (#623).
  function viewTokioStats(): void {
    const s = store.getState();
    if (s.diff.a && s.diff.b) {
      launchDiff("tokio");
      return;
    }
    const sel = s.browse.selection;
    if (!sel || !sel.keys.length) return;
    const bucket = els.bucketInput.value.trim();
    if (!bucket) {
      alert("Bucket is required");
      return;
    }
    const scope = scopeFromKeys(bucket, sel.keys, sel.t0, sel.t1, currentRegion(els));
    if (!scope) return; // window always present for a box selection
    const { query, hostsDropped } = encodeAggregationParams(new URLSearchParams(), scope);
    if (hostsDropped) warnHostsDropped();
    window.open("tokio_stats.html?" + query, "_blank");
  }

  // Span Explorer button: the same aggregation scope as the demand-driven
  // flamegraph and tokio-stats (/api/span-stats reads the same param
  // vocabulary). No diff route - the Span Explorer has no two-sided view.
  function viewSpanExplorer(): void {
    const sel = store.getState().browse.selection;
    if (!sel || !sel.keys.length) return;
    const bucket = els.bucketInput.value.trim();
    if (!bucket) {
      alert("Bucket is required");
      return;
    }
    const scope = scopeFromKeys(bucket, sel.keys, sel.t0, sel.t1, currentRegion(els));
    if (!scope) return; // window always present for a box selection
    const base = new URLSearchParams();
    base.set("api", "1");
    const { query, hostsDropped } = encodeAggregationParams(base, scope);
    if (hostsDropped) warnHostsDropped();
    window.open("span_explorer.html?" + query, "_blank");
  }

  return { viewSelected, viewCpuProfile, viewTokioStats, viewSpanExplorer };
}
