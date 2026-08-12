// session.js - privacy-preserving, tab-scoped request correlation.
//
// A random UUID is generated once per browser tab, persisted in
// sessionStorage, and attached to same-origin /api requests. The identifier is
// intentionally opaque: it contains no user, account, bucket, or trace data and
// disappears when the tab closes.
//
// Node-safe: tests can inject storage and UUID generation. No top-level browser
// globals are required.

(function (exports) {
  "use strict";

  const STORAGE_KEY = "dial9.session-id";
  const HEADER_NAME = "x-dial9-session-id";
  const UUID_RE =
    /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/;

  let injectedStorage = null;
  let injectedRandomUuid = null;
  let memoryId = null;

  function browserStorage() {
    if (injectedStorage) return injectedStorage;
    try {
      return typeof sessionStorage !== "undefined" ? sessionStorage : null;
    } catch {
      return null;
    }
  }

  function isValid(value) {
    return typeof value === "string" && UUID_RE.test(value);
  }

  function fallbackRandomUuid() {
    if (
      typeof crypto === "undefined" ||
      typeof crypto.getRandomValues !== "function"
    ) {
      return null;
    }
    const bytes = crypto.getRandomValues(new Uint8Array(16));
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    const hex = Array.from(bytes, (b) => b.toString(16).padStart(2, "0")).join(
      ""
    );
    return (
      hex.slice(0, 8) +
      "-" +
      hex.slice(8, 12) +
      "-" +
      hex.slice(12, 16) +
      "-" +
      hex.slice(16, 20) +
      "-" +
      hex.slice(20)
    );
  }

  function generate() {
    if (injectedRandomUuid) return injectedRandomUuid();
    if (typeof crypto !== "undefined" && typeof crypto.randomUUID === "function") {
      return crypto.randomUUID();
    }
    return fallbackRandomUuid();
  }

  /** Return this tab's session ID, generating and persisting it if necessary. */
  function get() {
    const store = browserStorage();
    if (store) {
      try {
        const stored = store.getItem(STORAGE_KEY);
        if (isValid(stored)) {
          memoryId = stored;
          return stored;
        }
      } catch {
        // Storage can be blocked by browser policy. The in-memory fallback
        // still gives the tab a stable ID for the lifetime of this page.
      }
    }

    if (isValid(memoryId)) return memoryId;
    const generated = generate();
    if (!isValid(generated)) return null;
    memoryId = generated;
    if (store) {
      try {
        store.setItem(STORAGE_KEY, generated);
      } catch {
        // Keep the in-memory ID when sessionStorage is unavailable or full.
      }
    }
    return generated;
  }

  /**
   * Add the session header to an existing plain header bag. A fresh object is
   * returned so callers' credential/header objects are never mutated.
   */
  function headers(base = {}) {
    const result = {};
    if (typeof Headers !== "undefined" && base instanceof Headers) {
      base.forEach((value, key) => {
        result[key] = value;
      });
    } else if (Array.isArray(base)) {
      for (const [key, value] of base) result[key] = value;
    } else {
      Object.assign(result, base);
    }
    const id = get();
    if (id) result[HEADER_NAME] = id;
    return result;
  }

  function isSameOriginApi(input) {
    const raw =
      typeof input === "string" ||
      (typeof URL !== "undefined" && input instanceof URL)
        ? String(input)
        : input.url;
    try {
      if (typeof location === "undefined" || !location.origin) {
        return raw === "/api" || raw.startsWith("/api/");
      }
      const url = new URL(raw, location.href);
      return (
        url.origin === location.origin &&
        (url.pathname === "/api" || url.pathname.startsWith("/api/"))
      );
    } catch {
      return false;
    }
  }

  /**
   * Fetch with the session header only for same-origin /api requests. Caller
   * headers are preserved, and an attempted caller-supplied session ID is
   * overwritten with this tab's generated ID.
   */
  function fetchWithSession(input, init = {}) {
    if (!isSameOriginApi(input)) return fetch(input, init);
    const requestHeaders =
      init.headers !== undefined
        ? init.headers
        : typeof Request !== "undefined" && input instanceof Request
          ? input.headers
          : {};
    return fetch(input, { ...init, headers: headers(requestHeaders) });
  }

  const Dial9Session = {
    get,
    headers,
    fetch: fetchWithSession,
    // Test seams.
    _setStorage(store) {
      injectedStorage = store;
      memoryId = null;
    },
    _setRandomUuid(randomUuid) {
      injectedRandomUuid = randomUuid;
      memoryId = null;
    },
  };

  if (typeof window !== "undefined") {
    window.Dial9Session = Dial9Session;
  }
  if (typeof module !== "undefined" && module.exports) {
    module.exports = { Dial9Session };
  } else if (typeof exports !== "undefined") {
    exports.Dial9Session = Dial9Session;
  }
})(typeof exports === "undefined" ? this : exports);
