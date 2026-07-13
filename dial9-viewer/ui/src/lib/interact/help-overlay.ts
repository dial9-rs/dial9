// The unified `?` help overlay. One component for every page; each page
// supplies its OWN content sections (only the bindings that work on that
// page). `?` toggles it from the page's key router; Escape and
// click-outside close it.
//
// Escape composition: the overlay grabs focus on open (root tabindex=-1)
// and consumes Escape locally with stopPropagation, so an open overlay
// closes WITHOUT running page-level Escape handlers. Because focus can
// still leave the overlay (mouse click on the page), pages must ALSO check
// isOpen() first in their own Escape handling.

import "../../styles/interact.css";

export interface HelpRow {
  /** Key label, rendered as <kbd> ("/", "n / p", "Cmd/Ctrl + F"). */
  keys: string;
  /** What the key does on this page. */
  text: string;
}

export interface HelpSection {
  /** Optional section heading ("Keyboard", "Mouse"). */
  title?: string;
  rows: readonly HelpRow[];
}

export interface HelpOverlayOptions {
  /** Dialog title (page-specific, e.g. "Flamegraph - keyboard"). */
  title: string;
  sections: readonly HelpSection[];
  /** Footer hint; defaults to the canonical dismiss instruction. */
  footer?: string;
}

export interface HelpOverlay {
  open(): void;
  close(): void;
  toggle(): void;
  isOpen(): boolean;
  /** Remove the overlay from the document. */
  dispose(): void;
}

const DEFAULT_FOOTER =
  "Press Esc or ? to close \u00b7 existing bindings unchanged";

/** Create the overlay, mounted hidden into `doc.body`. */
export function createHelpOverlay(
  doc: Document,
  options: HelpOverlayOptions,
): HelpOverlay {
  const backdrop = doc.createElement("div");
  backdrop.className = "d9-help-backdrop";

  const box = doc.createElement("div");
  box.className = "d9-help";
  box.setAttribute("role", "dialog");
  box.setAttribute("aria-modal", "true");
  box.setAttribute("aria-label", options.title);
  box.tabIndex = -1;

  // h2 title + h3 sections: the pages' headers carry the h1, so the
  // dialog's headings continue the outline without skipping levels.
  const heading = doc.createElement("h2");
  heading.textContent = options.title;
  box.appendChild(heading);

  for (const section of options.sections) {
    if (section.title !== undefined) {
      const h = doc.createElement("h3");
      h.textContent = section.title;
      box.appendChild(h);
    }
    for (const row of section.rows) {
      const div = doc.createElement("div");
      div.className = "d9-help-row";
      const kbd = doc.createElement("kbd");
      kbd.textContent = row.keys;
      const span = doc.createElement("span");
      span.textContent = row.text;
      div.appendChild(kbd);
      div.appendChild(span);
      box.appendChild(div);
    }
  }

  const foot = doc.createElement("div");
  foot.className = "d9-help-foot";
  foot.textContent = options.footer ?? DEFAULT_FOOTER;
  box.appendChild(foot);

  backdrop.appendChild(box);
  doc.body.appendChild(backdrop);

  let restoreFocus: HTMLElement | null = null;

  function close(): void {
    if (!backdrop.classList.contains("open")) return;
    backdrop.classList.remove("open");
    if (restoreFocus !== null) {
      restoreFocus.focus();
      restoreFocus = null;
    }
  }

  function open(): void {
    if (backdrop.classList.contains("open")) return;
    const active = doc.activeElement;
    restoreFocus = active instanceof HTMLElement ? active : null;
    backdrop.classList.add("open");
    box.focus();
  }

  // Click on the dark backdrop (not the content box) closes.
  backdrop.addEventListener("click", (e) => {
    if (e.target === backdrop) close();
  });

  box.addEventListener("keydown", (e) => {
    if (e.key === "Escape") {
      e.preventDefault();
      e.stopPropagation();
      close();
    }
  });

  return {
    open,
    close,
    toggle(): void {
      if (backdrop.classList.contains("open")) close();
      else open();
    },
    isOpen(): boolean {
      return backdrop.classList.contains("open");
    },
    dispose(): void {
      backdrop.remove();
    },
  };
}
