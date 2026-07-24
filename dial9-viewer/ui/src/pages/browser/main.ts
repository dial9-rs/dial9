// Browser page entry: the S3 trace browser as a Vite page module.
//
// Boot order: dual-UI switch mount, `?trace=` passthrough, state restore
// from the URL, then the /api/config bootstrap chain (creds UI -> region
// detect -> prefix discovery -> auto-search).

import "../../styles/browser.css";
import { createActions } from "./actions.js";
import type { ApiConfig } from "./api.js";
import { mountActionsBar } from "./actions-bar.js";
import { resolveBucketFilter } from "./bucket-filter.js";
import { mountBrowseView } from "./browse-view.js";
import { mountCredsPanel } from "./creds-panel.js";
import { mountDiffTray } from "./diff-tray.js";
import type { PageCtx } from "./ctx.js";
import { queryEls } from "./dom.js";
import { mountFooter } from "./footer.js";
import { dateToPickerStr } from "./format.js";
import { mountHeader } from "./header.js";
import { mountHeatmapInteraction } from "./heatmap-interact.js";
import { mountHeatmapKeys } from "./heatmap-keys.js";
import { mountBrowserPageKeys } from "./page-keys.js";
import { mountRawView } from "./raw-view.js";
import { mountSearchControls } from "./search-controls.js";
import { mountServiceTabs } from "./service-tabs.js";
import { mountSelectionOverlay } from "./selection-overlay.js";
import { createBrowserStore, type BrowserState } from "./state.js";
import { mountTabs } from "./tabs.js";

// Dual-UI switch: render the "Switch to legacy UI" control. The
// ui-switch.js <head> auto-boot is a no-op on this off-root path.
window.D9UiSwitch?.mount({ side: "new" });

// `?trace=` passthrough - redirect to the viewer preserving all params
// (including repeated `trace=` components). Relative target resolves to
// /viewer.html via <base href="/">. The rest of the boot is skipped:
// nothing observable renders before the replace lands, and it avoids
// firing stray /api requests from a page that is going away.
const qs = window.location.search;
if (new URLSearchParams(qs).has("trace")) {
  window.location.replace("viewer.html" + qs);
} else {
  boot();
}

function boot(): void {
  const els = queryEls();
  const store = createBrowserStore();
  const actions = createActions(store, els);
  const ctx: PageCtx = { store, els, actions };

  // Resolve the bucket-picker filter BEFORE the URL restore below - its
  // final syncUrl() rewrites the query string, and a `bucket_filter=`
  // override must be captured first (actions.syncUrl then re-appends it so
  // the override survives every history.replaceState).
  const bucketFilter = resolveBucketFilter(window.location.search);
  if (bucketFilter.override != null) {
    store.update("config", {
      bucketFilter: bucketFilter.filter,
      bucketFilterOverride: bucketFilter.override,
    });
  }

  // Mount order matters only for the two `browse` subscribers: the painter
  // (browse-view) must rebuild the host labels before the selection overlay
  // re-applies their `.sel` classes within the same flush.
  mountHeader(ctx);
  mountSearchControls(ctx);
  mountTabs(ctx);
  mountServiceTabs(ctx);
  mountBrowseView(ctx);
  mountSelectionOverlay(ctx);
  mountHeatmapInteraction(ctx);
  // Unified keyboard model: `?` help, `/` search focus, Enter submits,
  // heatmap keyboard window selection.
  mountBrowserPageKeys(ctx);
  mountHeatmapKeys(ctx);
  mountRawView(ctx);
  mountActionsBar(ctx);
  mountDiffTray(ctx);
  const credsPanel = mountCredsPanel(ctx);
  mountFooter(ctx);

  // Init: restore state from the URL. Every state-changing action mirrors
  // the page state into the query string via syncUrl(); restore that saved
  // state here. syncUrl() is suppressed while restoring, then run once at
  // the end.
  const urlState = window.Dial9UrlState.parse(window.location.search);
  actions.setRestoring(true);
  // Timezone first: the range pickers format in the active TZ, so it must
  // be set before any date is written below.
  if (urlState.tz === "local") {
    store.update("ui", { useLocalTz: true });
  }
  if (urlState.bucket) els.bucketInput.value = urlState.bucket;
  if (urlState.prefix) els.prefixInput.value = urlState.prefix;
  if (urlState.service) els.serviceInput.value = urlState.service;
  if (urlState.q) els.rawSearchInput.value = urlState.q;
  actions.mirrorPrefix();
  if (urlState.tab === "raw") actions.switchTab("raw");

  // A shared link can carry an assume-role ARN (aws_role_arn): "read these
  // traces as this role". Fold it into the store as the assume-role transport
  // so every /api/* request carries the role-arn header and the server assumes
  // it with its own identity. A role ARN grants nothing on its own, which is
  // why it's safe in a link; an invalid one throws (setRoleArn validates the
  // shape) - surface it rather than silently falling back to the server's
  // identity. Restored before the region below so setRegion patches the ARN.
  if (urlState.roleArn && window.Dial9Creds) {
    try {
      window.Dial9Creds.setRoleArn(urlState.roleArn, { region: urlState.region });
    } catch (e) {
      console.warn(
        "ignoring invalid aws_role_arn in URL:",
        e instanceof Error ? e.message : e,
      );
    }
  }

  // A shared link can pin the bucket's region (aws_region). Fold it into
  // the stored credentials so every subsequent request carries it as the
  // region header - this is what lets a cross-region bucket link load
  // without a fresh detection round trip. Prefill the panel field too so
  // it's visible. setRegion keeps the active transport's kind (a no-op when
  // nothing is stored, and when the ARN branch above already pinned it).
  if (urlState.region && window.Dial9Creds) {
    if (!els.credsRegion.value) els.credsRegion.value = urlState.region;
    const stored = window.Dial9Creds.get();
    if (stored && stored.region !== urlState.region) {
      window.Dial9Creds.setRegion(urlState.region);
    }
  }

  // Restore the time range. A relative `last=N` re-anchors to now (so a
  // shared link always shows the last N hours); a precise `from`/`to` is
  // restored verbatim; otherwise fall back to the last-1hr default.
  if (urlState.last) {
    actions.setQuickRange(urlState.last);
  } else if (urlState.from != null && urlState.to != null) {
    const tz = store.getState().ui.useLocalTz;
    els.rangeFrom.value = dateToPickerStr(new Date(urlState.from * 1000), tz);
    els.rangeTo.value = dateToPickerStr(new Date(urlState.to * 1000), tz);
  } else {
    actions.setQuickRange(1);
  }
  actions.setRestoring(false);
  // Normalize the URL once: collapses any malformed/duplicate params from
  // the incoming link into the canonical serialization.
  actions.syncUrl();

  // First paint: run every component's render once through the scheduler
  // so the initial DOM reflects the restored state (renders never run
  // outside a store notification tick).
  primeRenders(store);

  // Config bootstrap. Plain fetch, not apiFetch: /api/config is fetched
  // uncredentialed.
  fetch("/api/config")
    .then(async (r) => {
      if (!r.ok) {
        const body = await r.text().catch(() => "");
        throw new Error(`HTTP ${r.status}${body ? ": " + body : ""}`);
      }
      return r.json() as Promise<ApiConfig>;
    })
    .then((config) => {
      // The server's default bucket belongs to the server's identity. Don't
      // prefill it when the user has brought their own credentials - that
      // bucket isn't theirs; they pick from their own buckets instead.
      const usingByoCreds = !!(window.Dial9Creds && window.Dial9Creds.has());
      if (!els.bucketInput.value && config.default_bucket && !usingByoCreds) {
        els.bucketInput.value = config.default_bucket;
      }
      if (config.default_prefix) {
        if (!els.prefixInput.value) els.prefixInput.value = config.default_prefix;
        actions.mirrorPrefix();
        store.update("config", { serverHasPrefix: true });
      }
      // When the server runs demand-driven aggregation, the flamegraph
      // button drives the sampled server-side loop instead of decoding raw
      // traces; Tokio Stats enables on selection.
      store.update("config", { aggregationEnabled: !!config.aggregation_enabled });
      // Local-dir servers have no BYO credentials; their buffer-style keys
      // carry no scope, so selections open directly by key (#627).
      store.update("config", { localMode: !config.supports_byo_credentials });
      // The server's bucket-picker filter applies unless the page URL
      // pinned an override at load (which wins). Servers predating the
      // field leave the "dial9" default in place.
      if (
        config.bucket_filter != null &&
        store.getState().config.bucketFilterOverride == null
      ) {
        store.update("config", { bucketFilter: config.bucket_filter });
      }
      if (config.supports_byo_credentials) credsPanel.init();
      // Server defaults (bucket/prefix) were filled programmatically above;
      // reflect them in the URL so the link is complete.
      actions.syncUrl();
      // If we're restoring a bucket with credentials but no region pinned
      // in the URL, detect it before discovery/search so the first signed
      // call lands on the right regional endpoint. A pinned region makes
      // this a fast no-op.
      return actions
        .detectRegionForBucket(els.bucketInput.value.trim())
        .then(() => actions.discoverPrefixes())
        .then(() => actions.discoverServices());
    })
    .catch(() => {
      void actions.discoverPrefixes().then(() => actions.discoverServices());
    });

  window.addEventListener("popstate", () => {
    const state = window.Dial9UrlState.parse(window.location.search);
    const service = state.service ?? "";
    if (service) {
      actions.selectService(service, "replace");
    } else {
      actions.clearBrowseNoService();
    }
  });
}

/** Run every subscriber once so the first frame renders the initial state. */
function primeRenders(store: ReturnType<typeof createBrowserStore>): void {
  const slices: (keyof BrowserState)[] = [
    "ui",
    "config",
    "form",
    "search",
    "browse",
    "raw",
    "creds",
    "diff",
    "transient",
  ];
  for (const slice of slices) store.update(slice, {});
}
