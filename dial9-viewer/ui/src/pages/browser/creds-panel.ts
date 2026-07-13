// Bring-your-own-credentials panel component (features/01 B2 + C1-C11),
// ported from the legacy initCredsUi (index.html:452-687). CONDITIONAL on
// the server reporting supports_byo_credentials: mount() only registers the
// render subscription; init() - called from the config bootstrap - wires
// the handlers, exactly like the legacy function was only invoked then.
//
// The panel and the scripting API (window.Dial9Creds.set) share this exact
// apply path, so an injected userscript and a human exercise identical
// logic (C10). Credential VALUES live in creds.js/sessionStorage and the
// uncontrolled input fields; the store carries only the UI state (panel
// open, status line, bucket picker).

import { assertInScheduledRender } from "../../store/store.js";
import { bucketMatchesFilter } from "./bucket-filter.js";
import type { PageCtx } from "./ctx.js";

export interface CredsPanel {
  /** Wire the panel (legacy initCredsUi). Call once, from /api/config. */
  init(): void;
}

export function mountCredsPanel({ store, els, actions }: PageCtx): CredsPanel {
  // Buckets whose name matches the configured filter are the trace buckets
  // - surfaced by default; everything else is hidden unless "Show all" is
  // on (C6). T15 amendment: the predicate is config-driven (URL
  // `bucket_filter=` override > /api/config > "dial9" - bucket-filter.ts)
  // instead of the legacy hardcoded "dial9". An empty filter matches every
  // bucket, so filtering is effectively off and the toggle disappears.
  const isTraceBucket = (n: string) =>
    bucketMatchesFilter(n, store.getState().config.bucketFilter);

  function setStatus(msg: string | null, kind: "ok" | "error" | null): void {
    store.update("creds", { status: { text: msg || "", kind } });
  }

  // Reflect stored-credential state in the header button and panel inputs
  // (legacy refresh()).
  function refreshFields(): void {
    const creds = window.Dial9Creds;
    if (!creds) return;
    const stored = creds.get();
    store.update("creds", { active: creds.has() });
    if (stored) {
      els.credsAkid.value = stored.accessKeyId || "";
      els.credsSecret.value = stored.secretAccessKey || "";
      els.credsToken.value = stored.sessionToken || "";
      els.credsRegion.value = stored.region || "";
    }
  }

  function togglePanel(show?: boolean): void {
    const open = show ?? !store.getState().creds.panelOpen;
    store.update("creds", { panelOpen: open });
  }

  // Choose a bucket: fill the bucket field, detect its region via
  // /api/credentials/check, then run the search (legacy selectBucket).
  async function selectBucket(name: string): Promise<void> {
    const creds = window.Dial9Creds;
    if (!creds) return;
    store.update("creds", { selectedBucket: name });
    els.bucketInput.value = name;
    actions.syncUrl();
    setStatus(`Detecting region for ${name}…`, null);
    const result = await creds.check(name);
    if (result.ok) {
      if (result.region) {
        els.credsRegion.value = result.region;
        // Persist the resolved region so it rides on every request, then
        // mirror it into the URL (aws_region) so a shared link reproduces
        // the cross-region bucket. Legacy fired this without awaiting.
        void creds.set({
          accessKeyId: els.credsAkid.value,
          secretAccessKey: els.credsSecret.value,
          sessionToken: els.credsToken.value,
          region: result.region,
        });
        actions.syncUrl();
      }
      setStatus(`Using ${name}${result.region ? ` · region ${result.region}` : ""}`, "ok");
      void actions.discoverPrefixes().then(() => actions.reRunCurrentSearch());
    } else {
      setStatus(`Cannot use ${name}: ${result.error || "unknown error"}`, "error");
    }
  }

  // Auto-select when there's exactly one filter-matching bucket - the
  // common case - but not in the "show all" view, where the user is
  // browsing (legacy renderBucketPicker tail).
  function autoSelectSingleMatch(): void {
    const s = store.getState().creds;
    if (s.showAll) return;
    const matches = [...s.buckets].sort((a, b) => a.localeCompare(b)).filter(isTraceBucket);
    if (matches.length === 1) void selectBucket(matches[0]!);
  }

  // List the buckets the stored credentials can see and render the picker.
  // Used by Apply and on page load when credentials are already present
  // (legacy loadBuckets).
  async function loadBuckets(): Promise<void> {
    const creds = window.Dial9Creds;
    if (!creds) return;
    setStatus("Listing buckets…", null);
    try {
      const buckets = await creds.listBuckets();
      setStatus(`${buckets.length} bucket(s) — pick one below`, "ok");
      store.update("creds", { buckets, bucketsRowVisible: true });
      autoSelectSingleMatch();
    } catch (e) {
      setStatus("Rejected: " + (e instanceof Error ? e.message : String(e)), "error");
      store.update("creds", { bucketsRowVisible: false });
    }
  }

  // Render the bucket picker from the last listing (legacy
  // renderBucketPicker): filter-matching trace buckets by default, every
  // visible bucket when "Show all" is on, a toggle when the two lists
  // differ. The filter name renders into the messages ("dial9" by default,
  // byte-identical to the legacy strings).
  function renderPicker(
    buckets: readonly string[],
    showAll: boolean,
    selectedBucket: string | null,
  ): void {
    els.credsBuckets.textContent = "";
    const filter = store.getState().config.bucketFilter;

    const all = [...buckets].sort((a, b) => a.localeCompare(b));
    const matches = all.filter(isTraceBucket);
    const shown = showAll ? all : matches;

    function appendToggle(): void {
      if (all.length === matches.length) return;
      const t = document.createElement("button");
      t.textContent = showAll
        ? `Show ${filter} only (${matches.length})`
        : `Show all (${all.length})`;
      t.style.cssText = "font-style:italic;opacity:0.85";
      t.addEventListener("click", () => {
        const nowShowAll = !store.getState().creds.showAll;
        store.update("creds", { showAll: nowShowAll });
        // Legacy re-rendered the picker, whose tail auto-selects a single
        // filter match when returning to the filtered view.
        if (!nowShowAll) autoSelectSingleMatch();
      });
      els.credsBuckets.appendChild(t);
    }

    if (!shown.length) {
      els.credsBuckets.append(
        document.createTextNode(
          buckets.length
            ? `No ${filter} trace buckets visible to these credentials.`
            : "No buckets visible to these credentials.",
        ),
      );
      appendToggle();
      return;
    }

    for (const name of shown) {
      const b = document.createElement("button");
      b.textContent = name;
      // Highlight the trace buckets even within the full list.
      if (isTraceBucket(name)) b.classList.add("match");
      if (name === selectedBucket) b.classList.add("selected");
      b.addEventListener("click", () => {
        void selectBucket(name);
      });
      els.credsBuckets.appendChild(b);
    }
    appendToggle();
  }

  // Render subscription (registered at mount; everything renders from the
  // creds slice - plus the config slice's bucketFilter, which the picker
  // predicate/messages read - so the pre-init state - hidden button,
  // closed panel - is just the initial state).
  let lastPicker: {
    buckets: readonly string[];
    showAll: boolean;
    selectedBucket: string | null;
    bucketFilter: string;
  } | null = null;
  store.subscribe(["creds", "config"], (state) => {
    assertInScheduledRender("creds-panel render");
    const c = state.creds;
    els.credsBtn.style.display = c.enabled ? "" : "none";
    els.credsBtn.classList.toggle("active", c.active);
    els.credsBtnLabel.textContent = c.active ? "AWS Credentials ✓" : "AWS Credentials";
    els.credsPanel.style.display = c.panelOpen ? "" : "none";
    els.credsStatus.textContent = c.status.text;
    els.credsStatus.className = "creds-status" + (c.status.kind ? " " + c.status.kind : "");
    els.credsBucketsRow.style.display = c.bucketsRowVisible ? "" : "none";
    if (
      !lastPicker ||
      lastPicker.buckets !== c.buckets ||
      lastPicker.showAll !== c.showAll ||
      lastPicker.selectedBucket !== c.selectedBucket ||
      lastPicker.bucketFilter !== state.config.bucketFilter
    ) {
      lastPicker = {
        buckets: c.buckets,
        showAll: c.showAll,
        selectedBucket: c.selectedBucket,
        bucketFilter: state.config.bucketFilter,
      };
      renderPicker(c.buckets, c.showAll, c.selectedBucket);
    }
  });

  let initialized = false;
  function init(): void {
    if (initialized) return;
    initialized = true;
    const creds = window.Dial9Creds;
    if (!creds) return;

    store.update("creds", { enabled: true });

    els.credsBtn.addEventListener("click", () => togglePanel());
    els.credsClose.addEventListener("click", () => togglePanel(false));

    // Paste box: extract credentials from an STS / Isengard JSON blob into
    // the individual fields, then let the user review and click Apply (C2).
    els.credsPasteFill.addEventListener("click", () => {
      const text = els.credsPaste.value.trim();
      if (!text) {
        setStatus("Paste a credentials JSON blob first", "error");
        return;
      }
      let parsed: ReturnType<typeof creds.parse>;
      try {
        parsed = creds.parse(text);
      } catch (e) {
        setStatus("Could not parse: " + (e instanceof Error ? e.message : String(e)), "error");
        return;
      }
      els.credsAkid.value = parsed.accessKeyId || "";
      els.credsSecret.value = parsed.secretAccessKey || "";
      els.credsToken.value = parsed.sessionToken || "";
      if (parsed.region) els.credsRegion.value = parsed.region;
      // Don't leave the raw secret/token sitting in the textarea.
      els.credsPaste.value = "";
      setStatus("Filled from pasted JSON — review and click Apply", "ok");
    });

    // Apply (C4): validate, drop any prior bucket selection (it belongs to
    // a different identity), store the creds, list the visible buckets.
    els.credsApply.addEventListener("click", () => {
      void (async () => {
        if (!els.credsAkid.value.trim() || !els.credsSecret.value.trim()) {
          setStatus("Access key ID and secret are required", "error");
          return;
        }
        els.bucketInput.value = "";
        actions.syncUrl();
        // Store the credentials (no region detection yet - we don't have a
        // bucket). Then list the buckets these credentials can see; this
        // both validates the credentials and powers the picker.
        await creds.set({
          accessKeyId: els.credsAkid.value,
          secretAccessKey: els.credsSecret.value,
          sessionToken: els.credsToken.value,
          region: els.credsRegion.value,
          autoDetectRegion: false,
        });
        await loadBuckets();
      })();
    });

    // Clear (C5): wipe stored creds, fields, picker, and the browse pane
    // (the heatmap belonged to the removed identity's bucket).
    els.credsClear.addEventListener("click", () => {
      creds.clear();
      els.credsAkid.value = "";
      els.credsSecret.value = "";
      els.credsToken.value = "";
      els.credsRegion.value = "";
      // The selected bucket belonged to the now-removed identity; drop it
      // so a later search can't query a foreign bucket as the server's
      // ambient identity (which would 500/leak).
      els.bucketInput.value = "";
      actions.syncUrl();
      store.update("creds", {
        buckets: [],
        bucketsRowVisible: false,
        selectedBucket: null,
      });
      actions.resetBrowsePane();
      setStatus("Credentials cleared", null);
    });

    // React to programmatic changes (e.g. an injected userscript calling
    // Dial9Creds.set) as well as the panel's own actions: refresh the UI
    // and re-run the current search so results reflect the new credentials
    // (C10). Only re-run when credentials are now present - this event also
    // fires on Clear, where re-running would query the last-selected bucket
    // with no credentials and fail.
    window.addEventListener("dial9:credentials-changed", () => {
      refreshFields();
      if (actions.isAutoSearched() && creds.has()) actions.reRunCurrentSearch();
    });

    refreshFields();

    // Returning user (C9): credentials already in sessionStorage. Keep the
    // panel closed (the green button signals creds are active) but re-list
    // their buckets in the background so the picker is ready when opened.
    if (creds.has()) {
      void loadBuckets();
    }
  }

  return { init };
}
