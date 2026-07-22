// Controls bar component: bucket/prefix inputs, prefix suggestion chips,
// quick-range buttons, From/To pickers, Search button.

import { assertInScheduledRender } from "../../store/store.js";
import type { PageCtx } from "./ctx.js";

export function mountSearchControls({ store, els, actions }: PageCtx): void {
  // Keep the URL in sync as the user types; re-discover prefixes on commit
  // (change), detecting the bucket's region FIRST - discovery is itself a
  // signed S3 call.
  els.bucketInput.addEventListener("input", () => {
    actions.syncUrl();
  });
  els.bucketInput.addEventListener("change", () => {
    void (async () => {
      await actions.detectRegionForBucket(els.bucketInput.value.trim());
      void actions.discoverPrefixes();
      actions.syncUrl();
    })();
  });

  // Prefix edits gate the Search button and ride the URL.
  els.prefixInput.addEventListener("input", () => {
    actions.mirrorPrefix();
    actions.syncUrl();
  });

  // Quick-range buttons.
  for (const btn of els.quickBtns) {
    const hours = Number(btn.dataset["hours"]);
    btn.addEventListener("click", () => {
      actions.setQuickRange(hours);
    });
  }

  // A manual picker edit clears the quick-range highlight and flips the URL
  // from last=N to precise from/to.
  els.rangeFrom.addEventListener("input", () => {
    actions.clearQuickRange();
  });
  els.rangeTo.addEventListener("input", () => {
    actions.clearQuickRange();
  });

  // Run the time-range (Browse) search.
  els.searchBtn.addEventListener("click", () => {
    void actions.doTimeRangeSearch();
  });

  // Renders -------------------------------------------------------------

  let lastSuggestions: readonly string[] | null = null;

  store.subscribe(["search"], (state) => {
    assertInScheduledRender("search-controls render");
    const s = state.search;

    // Highlight: the data-hours markup attribute carries the quick-range
    // values.
    for (const btn of els.quickBtns) {
      btn.classList.toggle(
        "selected",
        s.quickRange != null && Number(btn.dataset["hours"]) === s.quickRange,
      );
    }

    // Placeholder state cycling.
    els.prefixInput.placeholder = s.prefixPlaceholder;

    // Suggestion chips: rebuild only when the discovered set changes; the
    // active highlight toggles in place (chip clicks move it, manual prefix
    // edits deliberately do not).
    if (s.suggestions !== lastSuggestions) {
      lastSuggestions = s.suggestions;
      els.prefixSuggestions.textContent = "";
      for (const label of s.suggestions) {
        const btn = document.createElement("button");
        btn.textContent = label;
        btn.style.cssText = "font-size:0.75em;padding:2px 8px";
        btn.addEventListener("click", () => {
          els.prefixInput.value = label;
          actions.mirrorPrefix();
          store.update("search", { activeSuggestion: label });
          actions.syncUrl();
        });
        els.prefixSuggestions.appendChild(btn);
      }
    }
    for (const btn of els.prefixSuggestions.querySelectorAll("button")) {
      btn.classList.toggle("active", btn.textContent === s.activeSuggestion);
    }
  });

  // Search readiness: disabled until a prefix is present when the server
  // declares one.
  store.subscribe(["config", "form"], (state) => {
    assertInScheduledRender("search-ready render");
    els.searchBtn.disabled =
      state.config.serverHasPrefix && state.form.prefix.trim() === "";
  });
}
