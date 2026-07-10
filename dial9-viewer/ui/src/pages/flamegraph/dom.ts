// src/pages/flamegraph/dom.ts - the flamegraph page's shell elements
// (T13). The Vite entry new/flamegraph.html carries the same static
// skeleton as the legacy page (header title/stats, container, loading,
// error); this module is the one place that resolves it, so a drifted
// shell fails loudly instead of null-dereferencing mid-load.

export interface PageEls {
  loadingEl: HTMLElement;
  errorEl: HTMLElement;
  containerEl: HTMLElement;
  titleEl: HTMLElement;
  statsEl: HTMLElement;
  /** Hide the loading indicator and show `msg` in the error element. */
  showError(msg: string): void;
}

function mustGet(id: string): HTMLElement {
  const el = document.getElementById(id);
  if (el === null) {
    throw new Error(`flamegraph shell is missing #${id}`);
  }
  return el;
}

/** Resolve the shell elements (throws on a drifted shell). */
export function pageEls(): PageEls {
  const loadingEl = mustGet("loading");
  const errorEl = mustGet("error");
  return {
    loadingEl,
    errorEl,
    containerEl: mustGet("fg-container"),
    titleEl: mustGet("fg-title"),
    statsEl: mustGet("fg-stats"),
    showError(msg: string): void {
      loadingEl.classList.add("hidden");
      errorEl.style.display = "flex";
      errorEl.textContent = msg;
    },
  };
}
