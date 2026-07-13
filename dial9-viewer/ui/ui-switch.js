// Dual-UI switch (T38; ADR-0004 section 8 stages 1-2).
//
// Included by every legacy page via ONE `<script src="ui-switch.js"></script>`
// line in <head> (the only edit legacy pages ever receive), and by future
// new-UI entries, which call `window.D9UiSwitch.mount({ side: "new" })`.
//
// What it does, on a canonical page URL (root-level /index.html, /viewer.html,
// /flamegraph.html, /tokio_stats.html, or "/"):
//
//   1. Resolve which UI the visitor gets. Precedence, highest first:
//        explicit `?ui=new` / `?ui=legacy` query param
//        > stored preference (localStorage, STORAGE_KEY below)
//        > DEFAULT_UI.
//   2. If "new" is resolved AND a new-UI entry is registered for this page
//      (NEW_UI_ENTRIES below), redirect there via location.replace, preserving
//      the query string (trace source) but not the hash.
//   3. Otherwise stay on the legacy page and, if a new-UI entry is registered,
//      render a small always-visible "Switch to new UI" control. When nothing
//      is registered, NO control renders (a switch to nowhere must not exist).
//      If the stay was forced by an explicit `?ui=legacy` pin against a
//      preference/default of "new", the "legacy" choice is persisted to the
//      stored preference - the legacy pages strip the pin on their first URL
//      sync, so the pin alone cannot be trusted to survive a reload (see
//      pinWouldBounce).
//
// New-UI entries live at their own dist paths (the canonical paths keep
// serving the legacy pages via the vite.config.ts static-copy list until
// legacy removal); they include this script and call mount({ side: "new" })
// to render the "Switch to legacy UI" control.
//
// The switch is RAW (maintainer decision 2026-07-08): NO view state ports
// across it. The query string is preserved on every switch/redirect - minus
// the `ui` param, which this script owns - because it carries the trace
// source; the hash (view state) is always dropped. Note the query string is
// re-serialized through URLSearchParams, so percent-encoding may normalize
// (values are preserved exactly; both UIs read params through the same
// decoding APIs). The control's target URL is resolved from the LIVE
// location at interaction time, never from a boot-time snapshot: the pages
// rewrite their query string as in-page state changes (see liveControlHref).
//
// Assumes the UI is served at the server root (true for `dial9 serve` and
// the dev-server): switch targets are built root-absolute.
//
// This is NOT a frozen-core file. Plain ES5-style browser JS (platform APIs:
// URLSearchParams, localStorage), no build step - legacy pages load it via a
// plain <script src>. The CommonJS guard at the bottom exposes the pure
// decision logic to vitest (tests/ui_switch.test.ts) via createRequire.
(function (exports) {
  "use strict";

  // Registry: canonical legacy page -> new-UI entry path (root-relative, no
  // leading slash). EMPTY until pages migrate.
  //
  // T13/T14/T41: registering your migrated page is ONE line here, e.g.:
  //   "flamegraph.html": "new/flamegraph.html",
  // The value must be the served dist path of your Vite entry. Registering a
  // page is what makes `?ui=new` work on its canonical URL and what makes the
  // "Switch to new UI" control appear on its legacy version.
  var NEW_UI_ENTRIES = {
    "flamegraph.html": "new/flamegraph.html", // T13
    // T14: the migrated S3 browser page (Vite entry new/index.html).
    "index.html": "new/index.html",
    // T21: the migrated trace-viewer shell (Vite entry new/viewer.html).
    "viewer.html": "new/viewer.html",
    // T41: the migrated Tokio Stats page (Vite entry new/tokio_stats.html).
    "tokio_stats.html": "new/tokio_stats.html",
  };

  // THE DEFAULT FLIP (ADR-0004 section 8, stage 2 -> stage 3): change
  // "legacy" to "new" on the next line to make the new UI the default for
  // every registered page. Deliberately left unflipped - maintainer decision.
  var DEFAULT_UI = "legacy";

  // localStorage key for the sticky preference. Values: "new" | "legacy".
  // Written when the user clicks the switch control; anything else (absent,
  // garbage, storage unavailable) reads as "no preference".
  var STORAGE_KEY = "dial9-ui-preference";

  // DOM id of the rendered control (T12's census asserts its presence).
  var CONTROL_ID = "d9-ui-switch";

  // ---------------------------------------------------------------------
  // Pure decision logic (no DOM, no storage access; unit-tested under node)
  // ---------------------------------------------------------------------

  // Which UI does the visitor get? Precedence: explicit ?ui= param >
  // stored preference > default. Unknown values fall through a level.
  function resolveUi(search, storedPref, defaultUi) {
    var param = new URLSearchParams(search).get("ui");
    if (param === "new" || param === "legacy") return param;
    if (storedPref === "new" || storedPref === "legacy") return storedPref;
    return defaultUi;
  }

  // Rebuild the query string for a switch target. Everything is preserved -
  // including repeated params like ?trace=a&trace=b (N10 deep links) - except
  // the `ui` param, which this script owns: removed when targeting the new
  // UI (the entry path itself selects it), pinned to `ui=legacy` when
  // targeting legacy (explicit, so the canonical page's dispatch cannot
  // bounce back to new even when localStorage is unavailable or stale).
  function buildQuery(search, targetUi) {
    var params = new URLSearchParams(search);
    params.delete("ui");
    if (targetUi === "legacy") params.set("ui", "legacy");
    var qs = params.toString();
    return qs ? "?" + qs : "";
  }

  // Registered new-UI entry for a canonical page, or null when the page has
  // no usable registration.
  // An entry that is itself a registry key would make the legacy-side
  // dispatch loop through location.replace() with no escape - a
  // self-registration ("x.html" -> "x.html") reloads forever, and a
  // cross-registration cycle ("a.html" -> "b.html" -> "a.html") hops
  // forever because buildQuery strips the `ui` param on every new-bound
  // hop, so not even `?ui=legacy` survives into the loop (T38 audit
  // finding 3). New-UI entries live off-root, so a legitimate entry is
  // never a key; treat any that is as unregistered.
  function registeredEntry(page, registry) {
    var entry =
      page && Object.prototype.hasOwnProperty.call(registry, page)
        ? registry[page]
        : null;
    if (entry && Object.prototype.hasOwnProperty.call(registry, entry)) {
      return null;
    }
    return entry;
  }

  // The decision function: (location + storage + registry) -> what happens.
  //
  // input: {
  //   side:       "legacy" | "new"  - which UI the calling page is,
  //   page:       canonical page name ("viewer.html") or null if unknown,
  //   search:     location.search ("" or "?..."),
  //   hash:       location.hash - accepted and DELIBERATELY unused: the
  //               switch is raw, hash view state never crosses it,
  //   storedPref: stored preference string or null,
  //   registry:   NEW_UI_ENTRIES (parameter for testability),
  //   defaultUi:  DEFAULT_UI (parameter for testability),
  // }
  //
  // Returns { redirect, control }:
  //   redirect: root-absolute URL to location.replace() to, or null,
  //   control:  { label, href, target } for the switch element, or null
  //             (null = render nothing).
  function decide(input) {
    var registry = input.registry;
    var page = input.page;
    var search = input.search || "";

    if (input.side === "new") {
      // New-UI side: never dispatches (the `?ui=` convention applies to the
      // canonical URL only); just offers the way back. Without a resolvable
      // canonical page there is nothing to switch to - render nothing.
      if (!page) return { redirect: null, control: null };
      return {
        redirect: null,
        control: {
          label: "Switch to legacy UI",
          href: "/" + page + buildQuery(search, "legacy"),
          target: "legacy",
        },
      };
    }

    // Legacy side.
    var newEntry = registeredEntry(page, registry);
    if (!newEntry) {
      // No migrated version registered: stay legacy, and hide the
      // affordance - a switch to nowhere must not render.
      return { redirect: null, control: null };
    }
    var newUrl = "/" + newEntry + buildQuery(search, "new");
    if (resolveUi(search, input.storedPref, input.defaultUi) === "new") {
      return { redirect: newUrl, control: null };
    }
    return {
      redirect: null,
      control: { label: "Switch to new UI", href: newUrl, target: "new" },
    };
  }

  // Canonical page name for a root-level pathname: "/" -> "index.html",
  // "/viewer.html" -> "viewer.html"; anything nested or non-.html -> null
  // (new-UI entries live off-root and are never treated as canonical).
  function canonicalPageFromPath(pathname) {
    if (pathname === "/" || pathname === "") return "index.html";
    var m = /^\/([^/]+\.html)$/.exec(pathname);
    return m ? m[1] : null;
  }

  // Reverse registry lookup for the new side: which canonical page does the
  // new-UI entry at `pathname` belong to?
  function pageForNewEntry(pathname, registry) {
    var served = pathname.replace(/^\//, "");
    for (var key in registry) {
      if (
        Object.prototype.hasOwnProperty.call(registry, key) &&
        registry[key] === served
      ) {
        return key;
      }
    }
    return null;
  }

  // The switch-control target resolved from the LIVE location (T38 audit
  // finding 1). Every legacy page rewrites its query string after boot -
  // history.replaceState/pushState of a rebuilt query (flamegraph browser
  // scope and worker zoom, tokio_stats hosts/periods, index browse state,
  // viewer start/end) - and the new SPA will too, so an href computed once
  // at boot navigates with a stale query and loses the current trace/scope.
  // The browser layer calls this at interaction time (mousedown + click)
  // and re-assigns the anchor's href just before it is used; the control's
  // label and direction stay boot-time, only the URL is live.
  //
  // The live pathname is re-resolved first (an SPA pushState may move it);
  // bootPage - the canonical page the control was mounted for - is the
  // fallback when the live pathname resolves to nothing. Returns the href,
  // or null when no target resolves (the caller then keeps the previous
  // href rather than producing a dead link).
  function liveControlHref(side, bootPage, pathname, search, registry) {
    var page =
      side === "new"
        ? pageForNewEntry(pathname, registry)
        : canonicalPageFromPath(pathname);
    if (!page) page = bootPage;
    if (!page) return null;
    if (side === "new") return "/" + page + buildQuery(search, "legacy");
    var entry = registeredEntry(page, registry);
    return entry ? "/" + entry + buildQuery(search, "new") : null;
  }

  // Is the explicit `?ui=legacy` pin the ONLY thing keeping this visitor on
  // the legacy UI (T38 audit finding 2)? Three of the four legacy pages
  // rebuild their query string from scratch on their first URL sync and
  // drop the unknown `ui` param (index, flamegraph, tokio_stats; only
  // viewer preserves it), so the pin cannot be relied on to survive. True
  // exactly when the same URL WITHOUT the pin would resolve "new" - i.e.
  // when losing the pin would bounce a reload (or a copied address-bar URL
  // in the same browser) back to the new UI against the visitor's explicit
  // choice. The browser layer then aligns the stored preference to
  // "legacy", making the choice survive the strip.
  //
  // Deliberately false when the visitor already resolves legacy without
  // the pin: a shared `?ui=legacy` link must not sticky-switch a recipient
  // whose preference/default is already legacy (mirroring the
  // write-on-click-only rule for `?ui=new` links). For a recipient whose
  // stored preference says "new" the audit weighs honoring the pinned
  // intent above the no-sticky rule - that conflict IS the bounce case.
  function pinWouldBounce(search, storedPref, defaultUi) {
    var params = new URLSearchParams(search);
    if (params.get("ui") !== "legacy") return false;
    params.delete("ui");
    var stripped = params.toString();
    return (
      resolveUi(stripped ? "?" + stripped : "", storedPref, defaultUi) ===
      "new"
    );
  }

  // Read a preference off a localStorage-like object. Failures (private
  // mode, storage disabled) and unknown values read as "no preference" -
  // never as a default UI choice.
  function prefFromStorage(storageLike) {
    try {
      var v = storageLike.getItem(STORAGE_KEY);
      return v === "new" || v === "legacy" ? v : null;
    } catch (e) {
      return null;
    }
  }

  exports.resolveUi = resolveUi;
  exports.buildQuery = buildQuery;
  exports.decide = decide;
  exports.canonicalPageFromPath = canonicalPageFromPath;
  exports.pageForNewEntry = pageForNewEntry;
  exports.liveControlHref = liveControlHref;
  exports.pinWouldBounce = pinWouldBounce;
  exports.prefFromStorage = prefFromStorage;
  exports.NEW_UI_ENTRIES = NEW_UI_ENTRIES;
  exports.DEFAULT_UI = DEFAULT_UI;
  exports.STORAGE_KEY = STORAGE_KEY;
  exports.CONTROL_ID = CONTROL_ID;

  // ---------------------------------------------------------------------
  // Browser layer (DOM + storage + navigation)
  // ---------------------------------------------------------------------

  if (typeof window === "undefined") return; // node/test environment

  function readStoredPref() {
    try {
      // Even touching window.localStorage can throw (SecurityError).
      return prefFromStorage(window.localStorage);
    } catch (e) {
      return null;
    }
  }

  function writeStoredPref(value) {
    try {
      window.localStorage.setItem(STORAGE_KEY, value);
    } catch (e) {
      // Best-effort: with storage unavailable the explicit `ui=` param on
      // legacy-bound URLs still keeps the choice honored (see buildQuery).
    }
  }

  // Render the small persistent switch control: a fixed-position pill in the
  // bottom-right corner, above everything, visible without scrolling on both
  // UIs. Built with createElement/textContent only - no URL-derived content
  // is ever interpolated into HTML.
  //
  // `liveHref` re-resolves the target URL from the live location (see
  // liveControlHref): the pages rewrite their query string after boot, so
  // the boot-time href is refreshed just before every use. mousedown runs
  // ahead of left- and middle-click navigation AND of the context menu
  // (so "Copy Link Address" copies the live URL); click covers keyboard
  // activation, which fires no mousedown.
  function mountControl(control, liveHref) {
    if (!control) return;
    function render() {
      if (document.getElementById(CONTROL_ID)) return; // idempotent
      var a = document.createElement("a");
      a.id = CONTROL_ID;
      a.textContent = control.label;
      a.href = control.href;
      function refreshHref() {
        var href = liveHref();
        if (href) a.href = href;
      }
      a.addEventListener("mousedown", refreshHref);
      a.addEventListener("click", function () {
        refreshHref();
        // Clicking the switch IS the preference. Navigation proceeds via
        // the (just refreshed) href - middle-click / copy-link work too;
        // those skip this handler, which is fine - see writeStoredPref.
        writeStoredPref(control.target);
      });
      var s = a.style;
      s.position = "fixed";
      s.right = "10px";
      s.bottom = "10px";
      s.zIndex = "2147483647";
      s.font =
        "12px -apple-system, BlinkMacSystemFont, 'Segoe UI', sans-serif";
      s.padding = "4px 10px";
      s.borderRadius = "12px";
      s.background = "rgba(20, 24, 30, 0.85)";
      s.color = "#e0e0e0";
      s.border = "1px solid rgba(224, 224, 224, 0.35)";
      s.textDecoration = "none";
      s.cursor = "pointer";
      document.body.appendChild(a);
    }
    if (document.readyState === "loading") {
      document.addEventListener("DOMContentLoaded", render);
    } else {
      render();
    }
  }

  function run(side, page) {
    var storedPref = readStoredPref();
    var result = decide({
      side: side,
      page: page,
      search: window.location.search,
      hash: window.location.hash, // dropped by decide(): raw switch
      storedPref: storedPref,
      registry: NEW_UI_ENTRIES,
      defaultUi: DEFAULT_UI,
    });
    if (result.redirect) {
      // replace(), not assign(): the pass-through legacy URL should not
      // pollute the back button.
      window.location.replace(result.redirect);
      return;
    }
    if (
      side === "legacy" &&
      result.control &&
      pinWouldBounce(window.location.search, storedPref, DEFAULT_UI)
    ) {
      // T38 audit finding 2: this visitor is on legacy ONLY because of the
      // explicit `?ui=legacy` pin, and the page's own URL syncs will strip
      // it. Persist the pinned choice so the next pin-less load still
      // resolves legacy instead of bouncing to the new UI. Gated on
      // result.control (a registered counterpart) - without one no
      // dispatch is possible, so a stray pin must not touch the global
      // preference. Best-effort: see writeStoredPref.
      writeStoredPref("legacy");
    }
    mountControl(result.control, function () {
      // T38 audit finding 1: the target is resolved from the LIVE location
      // at interaction time, never from the boot-time snapshot above.
      return liveControlHref(
        side,
        page,
        window.location.pathname,
        window.location.search,
        NEW_UI_ENTRIES
      );
    });
  }

  // Tiny API for new-UI entries: window.D9UiSwitch.mount({ side: "new" }).
  // The canonical page is found by reverse registry lookup on the current
  // pathname; pass { page: "viewer.html" } to override. Legacy pages never
  // call this - their <script src> include boots automatically below.
  function mount(opts) {
    opts = opts || {};
    var side = opts.side === "new" ? "new" : "legacy";
    var page = opts.page || null;
    if (!page) {
      page =
        side === "new"
          ? pageForNewEntry(window.location.pathname, NEW_UI_ENTRIES)
          : canonicalPageFromPath(window.location.pathname);
    }
    run(side, page);
  }

  window.D9UiSwitch = { mount: mount };

  // Auto-boot for the legacy pages (runs at parse time from <head>, so a
  // `?ui=new` dispatch replaces the page before its body scripts execute).
  // Off-root pathnames resolve to no canonical page and no registry entry,
  // so this is a no-op on new-UI entries that include the script - they call
  // mount({ side: "new" }) themselves.
  run("legacy", canonicalPageFromPath(window.location.pathname));
})(typeof exports === "undefined" ? {} : exports);
