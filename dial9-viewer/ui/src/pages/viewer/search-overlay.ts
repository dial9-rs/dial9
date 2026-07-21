// The viewer's `/` search palette: a centered modal over tasks / spans / points
// of interest. Pure-DOM (no lit) for its small, high-churn results list +
// keyboard model, mirroring the help overlay's modal mechanics (backdrop,
// focus-restore, local Escape) and registering in the Esc cascade.

import { searchQuery } from "./search-model.js";
import type { SearchIndex, SearchResult, SearchResults } from "./search-model.js";
import { ESC_PRIORITY } from "./esc-cascade.js";
import type { EscCascade } from "./esc-cascade.js";

export interface SearchOverlay {
  open(): void;
  close(): void;
  isOpen(): boolean;
  /** Detach the overlay and its Esc-cascade registration. */
  dispose(): void;
}

export interface SearchOverlayDeps {
  /** The current per-trace index, or null when no trace is loaded. */
  getIndex(): SearchIndex | null;
  /** Navigate to the picked result. The overlay has already closed itself. */
  onPick(result: SearchResult): void;
}

const PER_KIND = 8;

const GROUP_TITLE: Record<SearchResult["kind"], string> = {
  task: "Tasks",
  span: "Spans",
  poi: "Points of interest",
};

/** Mount the palette (hidden) into `doc.body` and register it in `esc`. */
export function mountViewerSearch(
  doc: Document,
  esc: EscCascade,
  deps: SearchOverlayDeps,
): SearchOverlay {
  const backdrop = doc.createElement("div");
  backdrop.className = "d9-search-backdrop";

  const box = doc.createElement("div");
  box.className = "d9-search";
  box.setAttribute("role", "dialog");
  box.setAttribute("aria-modal", "true");
  box.setAttribute("aria-label", "Search tasks, spans, and points of interest");

  const input = doc.createElement("input");
  input.className = "d9-search-input";
  input.type = "text";
  input.setAttribute("aria-label", "Search");
  input.placeholder = "Search tasks, spans, points of interest...";
  input.autocomplete = "off";
  input.spellcheck = false;

  const results = doc.createElement("div");
  results.className = "d9-search-results";
  results.setAttribute("role", "listbox");

  const foot = doc.createElement("div");
  foot.className = "d9-search-foot";
  foot.textContent = "Up / Down to move · Enter to jump · Esc to close";

  box.append(input, results, foot);
  backdrop.appendChild(box);
  doc.body.appendChild(backdrop);

  // Display-ordered results + their row elements + the active index (keyboard
  // nav + aria-activedescendant), rebuilt on every query.
  let flat: SearchResult[] = [];
  let rows: HTMLElement[] = [];
  let active = -1;
  let restoreFocus: HTMLElement | null = null;
  let rafPending = false;

  function isOpen(): boolean {
    return backdrop.classList.contains("open");
  }

  function messageOnly(text: string): void {
    results.textContent = "";
    flat = [];
    rows = [];
    active = -1;
    input.removeAttribute("aria-activedescendant");
    const el = doc.createElement("div");
    el.className = "d9-search-empty";
    el.textContent = text;
    results.appendChild(el);
  }

  function render(res: SearchResults): void {
    const q = input.value.trim();
    if (q === "") {
      messageOnly("Type to search tasks, spans, and points of interest.");
      return;
    }
    if (res.total === 0) {
      messageOnly(`No matches for "${q}".`);
      return;
    }
    results.textContent = "";
    flat = [];
    rows = [];
    const groups: readonly [SearchResult["kind"], readonly SearchResult[]][] = [
      ["task", res.tasks],
      ["span", res.spans],
      ["poi", res.pois],
    ];
    let idx = 0;
    for (const [kind, items] of groups) {
      if (items.length === 0) continue;
      const head = doc.createElement("div");
      head.className = "d9-search-group";
      head.textContent = GROUP_TITLE[kind];
      results.appendChild(head);
      for (const r of items) {
        const i = idx++;
        const row = doc.createElement("div");
        row.className = "d9-search-row";
        row.id = `d9-search-row-${i}`;
        row.setAttribute("role", "option");
        const badge = doc.createElement("span");
        badge.className = `d9-search-badge k-${kind}`;
        badge.textContent = kind === "poi" ? "POI" : kind;
        const label = doc.createElement("span");
        label.className = "d9-search-label";
        label.textContent = r.label;
        const sub = doc.createElement("span");
        sub.className = "d9-search-sub";
        sub.textContent = r.sublabel;
        row.append(badge, label, sub);
        // mousedown (not click) so the pick fires before the input blurs, and
        // preventDefault keeps focus in the input.
        row.addEventListener("mousedown", (e) => {
          e.preventDefault();
          pick(i);
        });
        results.appendChild(row);
        flat.push(r);
        rows.push(row);
      }
    }
    setActive(flat.length > 0 ? 0 : -1);
  }

  function runQuery(): void {
    const index = deps.getIndex();
    if (index === null) {
      messageOnly("No trace loaded.");
      return;
    }
    render(searchQuery(index, input.value, PER_KIND));
  }

  // Coalesce keystrokes to <= 1 query per frame (a fast typist never blocks on
  // an O(index) scan mid-keystroke).
  function scheduleQuery(): void {
    if (rafPending) return;
    rafPending = true;
    window.requestAnimationFrame(() => {
      rafPending = false;
      if (isOpen()) runQuery();
    });
  }

  function setActive(i: number): void {
    const prev = rows[active];
    if (prev) prev.classList.remove("active");
    active = i;
    const next = rows[active];
    if (next) {
      next.classList.add("active");
      next.scrollIntoView({ block: "nearest" });
      input.setAttribute("aria-activedescendant", next.id);
    } else {
      input.removeAttribute("aria-activedescendant");
    }
  }

  function move(delta: number): void {
    if (flat.length === 0) return;
    let i = active + delta;
    if (i < 0) i = flat.length - 1;
    else if (i >= flat.length) i = 0;
    setActive(i);
  }

  function pick(i: number): void {
    const r = flat[i];
    if (r === undefined) return;
    close();
    deps.onPick(r);
  }

  function open(): void {
    if (isOpen()) return;
    const a = doc.activeElement;
    restoreFocus = a instanceof HTMLElement ? a : null;
    backdrop.classList.add("open");
    input.value = "";
    runQuery();
    input.focus();
  }

  function close(): void {
    if (!isOpen()) return;
    backdrop.classList.remove("open");
    if (restoreFocus !== null) {
      restoreFocus.focus();
      restoreFocus = null;
    }
  }

  backdrop.addEventListener("mousedown", (e) => {
    if (e.target === backdrop) close();
  });
  input.addEventListener("input", scheduleQuery);
  box.addEventListener("keydown", (e) => {
    if (e.key === "Escape") {
      e.preventDefault();
      e.stopPropagation();
      close();
    } else if (e.key === "ArrowDown") {
      e.preventDefault();
      move(1);
    } else if (e.key === "ArrowUp") {
      e.preventDefault();
      move(-1);
    } else if (e.key === "Enter") {
      e.preventDefault();
      if (active >= 0) pick(active);
    }
  });

  const unregister = esc.register({
    name: "search",
    priority: ESC_PRIORITY.search,
    isOpen,
    close,
  });

  return {
    open,
    close,
    isOpen,
    dispose(): void {
      unregister();
      backdrop.remove();
    },
  };
}
