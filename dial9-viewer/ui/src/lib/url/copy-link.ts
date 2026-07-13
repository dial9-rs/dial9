// src/lib/url/copy-link.ts - the minimal copy-link action (T19; UX
// finding S3). A small toolbar button that copies the CURRENT page URL -
// after flushing any pending debounced view-state write, so the copied
// link carries the view the user is looking at, not the one from 150ms
// ago. The full share chrome (status bar) is chunk-2's; this button is
// what makes the mechanism usable on pages migrated at landing time.
//
// The page provides the mount point (its header/toolbar); this module
// owns the element, its inline look, and the copy mechanics. Styling is
// deliberately inline and minimal: the button must not disturb the
// migrated pages' pixel-parity headers beyond its own box.

export interface CopyLinkOptions {
  /** Run before reading the URL (pages pass the sync binding's flush). */
  beforeCopy?: () => void;
  /** URL supplier; defaults to the live location href. */
  getText?: () => string;
  /** Clipboard write; injectable for tests/fallbacks. */
  copy?: (text: string) => Promise<void>;
  /** Feedback flash duration, ms. */
  feedbackMs?: number;
}

export interface CopyLinkHandle {
  el: HTMLButtonElement;
  dispose(): void;
}

/**
 * Default clipboard write: the async Clipboard API, falling back to a
 * transient textarea + execCommand("copy") for non-secure contexts
 * (plain-http LAN hosts; the API exists only in secure contexts).
 */
async function defaultCopy(text: string): Promise<void> {
  if (navigator.clipboard !== undefined) {
    await navigator.clipboard.writeText(text);
    return;
  }
  const ta = document.createElement("textarea");
  ta.value = text;
  ta.setAttribute("readonly", "");
  ta.style.position = "fixed";
  ta.style.left = "-9999px";
  document.body.appendChild(ta);
  ta.select();
  try {
    if (!document.execCommand("copy")) {
      throw new Error("execCommand('copy') refused");
    }
  } finally {
    ta.remove();
  }
}

const LABEL = "Copy link";

/** Mount the copy-link button into `container` (appended last). */
export function mountCopyLink(
  container: HTMLElement,
  options: CopyLinkOptions = {},
): CopyLinkHandle {
  const doc = container.ownerDocument;
  const btn = doc.createElement("button");
  btn.type = "button";
  btn.className = "d9-copy-link";
  btn.textContent = LABEL;
  btn.title = "Copy a link to this exact view";
  // Minimal, header-neutral look; pushed to the header's right edge.
  btn.style.cssText =
    "margin-left:auto;padding:2px 8px;font:inherit;color:#e0e0e0;" +
    "background:#0f3460;border:1px solid #533483;border-radius:4px;" +
    "cursor:pointer;flex-shrink:0;";

  let feedbackTimer: ReturnType<typeof setTimeout> | null = null;
  function flash(label: string): void {
    btn.textContent = label;
    if (feedbackTimer !== null) clearTimeout(feedbackTimer);
    feedbackTimer = setTimeout(() => {
      feedbackTimer = null;
      btn.textContent = LABEL;
    }, options.feedbackMs ?? 1200);
  }

  const onClick = (): void => {
    options.beforeCopy?.();
    const text = options.getText !== undefined ? options.getText() : window.location.href;
    const copy = options.copy ?? defaultCopy;
    copy(text).then(
      () => {
        flash("Copied");
      },
      (err: unknown) => {
        // One-shot user action - loud console note, soft UI note.
        console.warn("copy-link: clipboard write failed:", err);
        flash("Copy failed");
      },
    );
  };
  btn.addEventListener("click", onClick);
  container.appendChild(btn);

  return {
    el: btn,
    dispose() {
      btn.removeEventListener("click", onClick);
      if (feedbackTimer !== null) clearTimeout(feedbackTimer);
      btn.remove();
    },
  };
}
