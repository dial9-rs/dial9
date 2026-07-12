// lib/interact/palette.ts - the unified search palette (T20; K1;
// canonical spec docs/ui-inventory/mocks/keyboard.html).
//
// One palette component for every page: `/` opens it, typing filters,
// ArrowUp/ArrowDown move the selection (clamped, mock semantics), Enter
// activates the selected item and CYCLES the selection forward (wrapping)
// so repeated Enter steps through the matches; Shift+Enter cycles
// backward ("Enter jump - Shift+Enter prev" in the mock's placeholder).
// Escape closes. The palette indexes whatever its opener supplies -
// tasks/spans/POIs come from lib/trace/query on the pages that have a
// parsed trace (the chunk-2 viewer; T23 wires it). Per architecture 2.5
// the component translates keys into the onActivate callback only - what
// an activation DOES (center viewport, select task) is the caller's.
//
// The filtering + selection-stepping policy is pure and Node-testable
// (filterPaletteItems / stepClamped / stepWrapped); the component is the
// thin DOM shell around it.
//
// Keydown inside the palette stops propagation (mock line 96): palette
// typing must never reach page-level routers or the frozen flamegraph
// widget's document-level `/` handler.

import "../../styles/interact.css";

/** One searchable item. The palette renders; the caller interprets. */
export interface PaletteItem {
  /** Kind tag rendered in the left column ("task", "span", "poi", ...). */
  kind: string;
  /** Primary display text; the filter matches on `kind` + `label`. */
  label: string;
  /** Optional right-aligned detail (the mock renders a time, "+0.31s"). */
  detail?: string;
}

/**
 * Case-insensitive substring filter over `"<kind> <label>"` - exactly the
 * mock's matching rule, so "poi sched" narrows by kind and text at once.
 * An empty/whitespace query keeps everything.
 */
export function filterPaletteItems<T extends PaletteItem>(
  items: readonly T[],
  query: string,
): T[] {
  const q = query.trim().toLowerCase();
  if (q === "") return [...items];
  return items.filter((it) =>
    (it.kind + " " + it.label).toLowerCase().includes(q),
  );
}

/** Arrow-key selection step: clamped to [0, length-1] (mock semantics). */
export function stepClamped(index: number, delta: number, length: number): number {
  if (length <= 0) return 0;
  return Math.min(Math.max(index + delta, 0), length - 1);
}

/** Enter-cycling selection step: wraps around the result list. */
export function stepWrapped(index: number, delta: number, length: number): number {
  if (length <= 0) return 0;
  return (((index + delta) % length) + length) % length;
}

export interface PaletteOptions {
  /** Input placeholder; defaults to the canonical mock hint text. */
  placeholder?: string;
  /** Called on Enter with the activated item. The palette stays open. */
  onActivate(item: PaletteItem): void;
}

export interface SearchPalette {
  /** Open (or refresh) the palette over `items`; focuses the input. */
  open(items: readonly PaletteItem[]): void;
  close(): void;
  isOpen(): boolean;
  /** Remove the palette from the document. */
  dispose(): void;
}

const DEFAULT_PLACEHOLDER =
  "search tasks, spans, POIs...  (Enter jump \u00b7 Shift+Enter prev \u00b7 Esc close)";

/** Create the palette, mounted hidden into `doc.body`. */
export function createSearchPalette(
  doc: Document,
  options: PaletteOptions,
): SearchPalette {
  const root = doc.createElement("div");
  root.className = "d9-palette";
  root.setAttribute("role", "dialog");
  root.setAttribute("aria-modal", "true");
  root.setAttribute("aria-label", "Search");

  const input = doc.createElement("input");
  input.type = "text";
  input.placeholder = options.placeholder ?? DEFAULT_PLACEHOLDER;
  input.setAttribute("role", "combobox");
  input.setAttribute("aria-expanded", "true");
  input.setAttribute("aria-autocomplete", "list");
  input.setAttribute("aria-label", "Search");

  const list = doc.createElement("div");
  list.className = "d9-palette-results";
  list.setAttribute("role", "listbox");
  list.id = "d9-palette-results";
  input.setAttribute("aria-controls", list.id);

  root.appendChild(input);
  root.appendChild(list);
  doc.body.appendChild(root);

  let allItems: readonly PaletteItem[] = [];
  let filtered: PaletteItem[] = [];
  let selected = 0;
  let restoreFocus: HTMLElement | null = null;

  function render(): void {
    list.textContent = "";
    if (filtered.length === 0) {
      const empty = doc.createElement("div");
      empty.className = "d9-palette-empty";
      empty.textContent = "no matches";
      list.appendChild(empty);
      input.removeAttribute("aria-activedescendant");
      return;
    }
    filtered.forEach((item, i) => {
      const row = doc.createElement("div");
      row.className = "d9-palette-row";
      row.setAttribute("role", "option");
      row.id = `d9-palette-opt-${i}`;
      row.setAttribute("aria-selected", i === selected ? "true" : "false");

      const kind = doc.createElement("span");
      kind.className = "d9-palette-kind";
      kind.textContent = item.kind;
      row.appendChild(kind);
      row.appendChild(doc.createTextNode(item.label));
      if (item.detail !== undefined) {
        const detail = doc.createElement("span");
        detail.className = "d9-palette-detail";
        detail.textContent = item.detail;
        row.appendChild(detail);
      }
      // Mouse path: keys are accelerators, never the only path (mock
      // status bar). Click activates and closes.
      row.addEventListener("click", () => {
        options.onActivate(item);
        close();
      });
      list.appendChild(row);
    });
    input.setAttribute("aria-activedescendant", `d9-palette-opt-${selected}`);
    list.children[selected]?.scrollIntoView({ block: "nearest" });
  }

  function setSelected(i: number): void {
    selected = i;
    render();
  }

  function close(): void {
    root.classList.remove("open");
    if (restoreFocus !== null) {
      restoreFocus.focus();
      restoreFocus = null;
    }
  }

  input.addEventListener("input", () => {
    filtered = filterPaletteItems(allItems, input.value);
    setSelected(0);
  });

  root.addEventListener("keydown", (e) => {
    // Palette typing is palette-private (see the module header).
    e.stopPropagation();
    if (e.key === "ArrowDown") {
      e.preventDefault();
      setSelected(stepClamped(selected, 1, filtered.length));
    } else if (e.key === "ArrowUp") {
      e.preventDefault();
      setSelected(stepClamped(selected, -1, filtered.length));
    } else if (e.key === "Enter") {
      e.preventDefault();
      const item = filtered[selected];
      if (item !== undefined) {
        options.onActivate(item);
        setSelected(stepWrapped(selected, e.shiftKey ? -1 : 1, filtered.length));
      }
    } else if (e.key === "Escape") {
      e.preventDefault();
      close();
    }
  });

  return {
    open(items: readonly PaletteItem[]): void {
      allItems = items;
      filtered = filterPaletteItems(allItems, "");
      selected = 0;
      input.value = "";
      const active = doc.activeElement;
      restoreFocus = active instanceof HTMLElement ? active : null;
      root.classList.add("open");
      render();
      input.focus();
    },
    close,
    isOpen(): boolean {
      return root.classList.contains("open");
    },
    dispose(): void {
      root.remove();
    },
  };
}
