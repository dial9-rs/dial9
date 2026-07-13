// Status announcements for keyboard actions. Two channels per
// announcement:
//   - an offscreen aria-live="polite" region, so screen readers hear the
//     result of a key that produced no focus change (zoom undo, fit,
//     selection started/confirmed);
//   - a transient visible toast (1.6s), so sighted keyboard users get the
//     same confirmation without hunting for what changed.
//
// One shared instance per page (lazily mounted); announcements are
// feedback only - no page logic may depend on them.

import "../../styles/interact.css";

export interface Announcer {
  announce(msg: string): void;
}

const TOAST_MS = 1600;

/** Create an announcer bound to `doc` (own instance; see getAnnouncer). */
export function createAnnouncer(doc: Document): Announcer {
  const region = doc.createElement("div");
  region.className = "d9-announce-region";
  region.setAttribute("aria-live", "polite");
  doc.body.appendChild(region);

  const toast = doc.createElement("div");
  toast.className = "d9-toast";
  doc.body.appendChild(toast);

  let hideTimer: ReturnType<typeof setTimeout> | null = null;
  return {
    announce(msg: string): void {
      region.textContent = msg;
      toast.textContent = msg;
      toast.classList.add("show");
      if (hideTimer !== null) clearTimeout(hideTimer);
      hideTimer = setTimeout(() => {
        toast.classList.remove("show");
      }, TOAST_MS);
    },
  };
}

let shared: Announcer | null = null;

/** The page's shared announcer (mounted into document.body on first use). */
export function getAnnouncer(): Announcer {
  if (shared === null) {
    shared = createAnnouncer(document);
  }
  return shared;
}
