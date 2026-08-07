// A reusable lifecycle wrapper around the frozen flamegraph widget.
//
// The widget (flamegraph.js) has no ResizeObserver of its own and re-reads its
// parent's clientWidth on every render, so a host that changes size without a
// re-attach or a setData keeps drawing at a stale width. It also must not be
// re-`setData`'d on every surrounding render, or a re-entrant redraw (the
// widget's own onZoom fires the caller's sync) loops. The region panel worked
// this out once inline; the inspector now needs the same discipline for a
// SECOND instance, so the shared bits live here rather than being copied.
//
// One host owns one lazily-created instance. `sync` is the whole contract:
// attach into the current DOM host (lit-html destroys and recreates that node
// on re-render), apply data only when the signature changed, and resize on a
// rAF whenever either happened.

import { createFlamegraph } from "../../lib/canvas/flamegraph.js";
import type { FlamegraphInstance } from "../../lib/canvas/flamegraph.js";

export interface FlamegraphHostOptions {
  /** Document the container is created in (host.ownerDocument at the call site). */
  doc: Document;
  /** CSS class for the widget's container element. */
  className: string;
  /** Forwarded to createFlamegraph; fires when the widget's zoom stack changes. */
  onZoom?: () => void;
}

export interface FlamegraphSyncArgs {
  /** The current DOM slot to render into (a binding-free host node). */
  hostEl: HTMLElement;
  /**
   * A string that changes exactly when the data would change. `apply` runs only
   * when it differs from the last applied signature, so an unrelated re-render
   * never rebuilds the tree.
   */
  sig: string;
  /** Push data into the instance (typically one `instance.setData(...)`). */
  apply: (instance: FlamegraphInstance) => void;
}

export interface FlamegraphHost {
  /**
   * Attach into `hostEl`, apply data when `sig` changed, and resize on a rAF if
   * either the host or the data moved. Idempotent for a stable host + sig.
   */
  sync(args: FlamegraphSyncArgs): void;
  /**
   * The panel left the flamegraph mode: the widget stays attached to its old
   * (now-removed) host, so drop the attach marker and stop observing. A later
   * `sync` re-attaches and resizes.
   */
  detach(): void;
  /** The live instance, or null before the first sync. */
  instance(): FlamegraphInstance | null;
  /** Destroy the widget and release the observer. */
  destroy(): void;
}

export function createFlamegraphHost(opts: FlamegraphHostOptions): FlamegraphHost {
  let container: HTMLElement | null = null;
  let instance: FlamegraphInstance | null = null;
  let lastHost: HTMLElement | null = null;
  let observer: ResizeObserver | null = null;
  let appliedSig: string | null = null;

  function ensure(): FlamegraphInstance {
    if (container === null || instance === null) {
      container = opts.doc.createElement("div");
      container.className = opts.className;
      instance = createFlamegraph(container, opts.onZoom ?? (() => {}));
    }
    return instance;
  }

  /**
   * Observe `host` for size changes, redrawing the canvas on a rAF. The resize
   * is deferred because ResizeObserver callbacks run inside the layout step,
   * before paint, and the widget reads clientWidth on the parent - resizing
   * inline risks a width the browser is still settling. Re-pointed at each new
   * host, so at most one element is ever observed.
   */
  function observe(host: HTMLElement, inst: FlamegraphInstance): void {
    const win = host.ownerDocument.defaultView ?? window;
    if (typeof win.ResizeObserver === "undefined") return;
    if (observer === null) {
      let pending = false;
      observer = new win.ResizeObserver(() => {
        if (pending) return;
        pending = true;
        win.requestAnimationFrame(() => {
          pending = false;
          // A resize can arrive after the panel switched away, at which point
          // the widget is detached and has nothing to redraw.
          if (lastHost !== null) inst.resize();
        });
      });
    }
    observer.disconnect();
    observer.observe(host);
  }

  return {
    sync({ hostEl, sig, apply }: FlamegraphSyncArgs): void {
      const inst = ensure();
      let needResize = false;
      if (container !== null && hostEl !== lastHost) {
        hostEl.appendChild(container);
        lastHost = hostEl;
        observe(hostEl, inst);
        needResize = true;
      }
      if (sig !== appliedSig) {
        // Set BEFORE apply: the widget's onZoom can fire the caller's sync
        // re-entrantly during setData, and it must see the signature satisfied
        // rather than loop back into another apply.
        appliedSig = sig;
        apply(inst);
        needResize = true;
      }
      if (needResize) {
        const win = hostEl.ownerDocument.defaultView ?? window;
        win.requestAnimationFrame(() => inst.resize());
      }
    },
    detach(): void {
      lastHost = null;
      observer?.disconnect();
    },
    instance(): FlamegraphInstance | null {
      return instance;
    },
    destroy(): void {
      observer?.disconnect();
      observer = null;
      lastHost = null;
      appliedSig = null;
      instance?.destroy();
      container?.remove();
      container = null;
      instance = null;
    },
  };
}
