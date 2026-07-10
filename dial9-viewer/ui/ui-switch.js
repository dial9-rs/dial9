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
// decoding APIs).
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
    // (no migrated pages yet)
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
    var newEntry =
      page && Object.prototype.hasOwnProperty.call(registry, page)
        ? registry[page]
        : null;
    // A self-registration ("x.html" -> "x.html") would make the redirect
    // below reload the page forever; treat it as unregistered.
    if (newEntry === page) newEntry = null;
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
  function mountControl(control) {
    if (!control) return;
    function render() {
      if (document.getElementById(CONTROL_ID)) return; // idempotent
      var a = document.createElement("a");
      a.id = CONTROL_ID;
      a.textContent = control.label;
      a.href = control.href;
      a.addEventListener("click", function () {
        // Clicking the switch IS the preference. Navigation proceeds via
        // the href (middle-click / copy-link work too; those skip this
        // handler, which is fine - see writeStoredPref).
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
    var result = decide({
      side: side,
      page: page,
      search: window.location.search,
      hash: window.location.hash, // dropped by decide(): raw switch
      storedPref: readStoredPref(),
      registry: NEW_UI_ENTRIES,
      defaultUi: DEFAULT_UI,
    });
    if (result.redirect) {
      // replace(), not assign(): the pass-through legacy URL should not
      // pollute the back button.
      window.location.replace(result.redirect);
      return;
    }
    mountControl(result.control);
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
