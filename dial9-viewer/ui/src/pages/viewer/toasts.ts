// The viewer's toast/notification channel. Imperative chrome: toasts are
// transient feedback that no page logic depends on, so they are a small stateful
// manager mounted into a container, not a store slice (nothing subscribes to a
// toast).
//
// Behavior:
//   - show(): create OR update a toast by id; a duplicate id re-triggers a
//     wiggle instead of stacking a second copy;
//   - types info|warn|error with the entry animation;
//   - persistent toasts survive clear();
//   - hide(id) / clear() removes non-persistent toasts.
//
// The container is created with role="status" + aria-live="polite" so the screen
// reader hears error/info toasts.

export type ToastType = "info" | "warn" | "error";

export interface ShowToastOptions {
  /** Stable id; a repeat show of the same id wiggles instead of stacking. */
  id: string;
  message: string;
  /** Visual/semantic kind; defaults to "info". */
  type?: ToastType;
  /** Auto-remove after N ms; omit/0 to leave until hide/clear. */
  autoHideMs?: number;
  /** Survive clear() (persistent hints). */
  persistent?: boolean;
}

export interface Toasts {
  show(options: ShowToastOptions): void;
  hide(id: string): void;
  /** Remove every non-persistent toast. */
  clear(): void;
  /** Remove the container from the document (teardown/tests). */
  dispose(): void;
}

const WIGGLE_MS = 400;

interface ToastEntry {
  el: HTMLElement;
  persistent: boolean;
  hideTimer: ReturnType<typeof setTimeout> | null;
  wiggleTimer: ReturnType<typeof setTimeout> | null;
}

/**
 * Mount a toast manager into `container` (an element already in the
 * document; the shell provides the labeled region). One manager per page.
 */
export function createToasts(container: HTMLElement): Toasts {
  const entries = new Map<string, ToastEntry>();

  function remove(id: string): void {
    const entry = entries.get(id);
    if (!entry) return;
    if (entry.hideTimer !== null) clearTimeout(entry.hideTimer);
    if (entry.wiggleTimer !== null) clearTimeout(entry.wiggleTimer);
    entry.el.remove();
    entries.delete(id);
  }

  return {
    show(options: ShowToastOptions): void {
      const type = options.type ?? "info";
      const existing = entries.get(options.id);
      if (existing) {
        // Duplicate id -> refresh text + wiggle, never a second copy.
        existing.el.textContent = options.message;
        existing.el.className = `d9-toast-item d9-toast-${type}`;
        existing.el.classList.add("d9-toast-wiggle");
        if (existing.wiggleTimer !== null) clearTimeout(existing.wiggleTimer);
        existing.wiggleTimer = setTimeout(() => {
          existing.el.classList.remove("d9-toast-wiggle");
          existing.wiggleTimer = null;
        }, WIGGLE_MS);
        return;
      }
      const el = container.ownerDocument.createElement("div");
      el.className = `d9-toast-item d9-toast-${type} d9-toast-in`;
      el.textContent = options.message;
      container.appendChild(el);
      const entry: ToastEntry = {
        el,
        persistent: options.persistent ?? false,
        hideTimer: null,
        wiggleTimer: null,
      };
      if (options.autoHideMs && options.autoHideMs > 0) {
        entry.hideTimer = setTimeout(() => remove(options.id), options.autoHideMs);
      }
      entries.set(options.id, entry);
    },
    hide(id: string): void {
      remove(id);
    },
    clear(): void {
      for (const [id, entry] of [...entries]) {
        if (!entry.persistent) remove(id);
      }
    },
    dispose(): void {
      for (const id of [...entries.keys()]) remove(id);
    },
  };
}
