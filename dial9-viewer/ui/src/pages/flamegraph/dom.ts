// The flamegraph page's shell elements: the one place that resolves the static
// skeleton, so a drifted shell fails loudly instead of null-dereferencing.

export interface PageEls {
  loadingEl: HTMLElement;
  errorEl: HTMLElement;
  containerEl: HTMLElement;
  titleEl: HTMLElement;
  statsEl: HTMLElement;
  /** Page header bar; page-level controls (copy-link) mount here. */
  headerEl: HTMLElement;
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
    headerEl: mustGet("fg-header"),
    showError(msg: string): void {
      loadingEl.classList.add("hidden");
      errorEl.style.display = "flex";
      errorEl.textContent = msg;
    },
  };
}
