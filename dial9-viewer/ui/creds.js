// creds.js — bring-your-own-credentials store for the dial9 viewer.
//
// The user pastes temporary, scoped AWS credentials into the viewer; they are
// kept in sessionStorage and attached as `x-dial9-aws-*` headers on every
// `/api/*` request so the backend can call S3 on the user's behalf without
// holding any standing access.
//
// This module is also the stable scripting entry point: an external userscript
// (e.g. one injected at AWS that auto-loads account credentials) drives the
// same code path the "Apply" button uses, via `window.Dial9Creds.set(...)`.
//
// Node-safe: the storage backend is injectable, so the test suite can exercise
// get/set/clear/headers without a browser. No top-level `window`/`sessionStorage`
// references.

(function (exports) {
  "use strict";

  const STORAGE_KEY = "dial9.aws-credentials";

  // Header names — must match src/server/credentials.rs.
  const H = {
    accessKeyId: "x-dial9-aws-access-key-id",
    secretAccessKey: "x-dial9-aws-secret-access-key",
    sessionToken: "x-dial9-aws-session-token",
    region: "x-dial9-aws-region",
    // Assume-role path: name a role for the server to assume with its own
    // identity, instead of supplying static keys. Alternative to the four above.
    roleArn: "x-dial9-aws-role-arn",
  };

  // Resolve a storage backend. In the browser this is sessionStorage (creds die
  // when the tab closes). Tests inject a fake via `Dial9Creds._setStorage(...)`.
  // If no storage exists (Node without injection), fall back to an in-memory map
  // so the API never throws.
  function memoryStorage() {
    const m = new Map();
    return {
      getItem: (k) => (m.has(k) ? m.get(k) : null),
      setItem: (k, v) => m.set(k, String(v)),
      removeItem: (k) => m.delete(k),
    };
  }

  let injectedStorage = null;
  function storage() {
    if (injectedStorage) return injectedStorage;
    if (typeof sessionStorage !== "undefined") return sessionStorage;
    injectedStorage = memoryStorage();
    return injectedStorage;
  }

  /** Read the stored credentials, or null if none are set. */
  function get() {
    try {
      const raw = storage().getItem(STORAGE_KEY);
      return raw ? JSON.parse(raw) : null;
    } catch {
      return null;
    }
  }

  /**
   * True if a usable credential set is stored. That is either a static
   * bring-your-own key pair, OR an assume-role ARN — both let the backend reach
   * S3 on the user's behalf, so both count as "credentials are active" for the
   * UI (green button, region header, re-run on change).
   */
  function has() {
    const c = get();
    return !!(c && ((c.accessKeyId && c.secretAccessKey) || c.roleArn));
  }

  /** Persist credentials and notify listeners. Returns the stored object.
   *
   * @param {{accessKeyId?, secretAccessKey?, sessionToken?, region?, roleArn?}} creds
   */
  function store(creds) {
    const clean = {
      accessKeyId: (creds.accessKeyId || "").trim(),
      secretAccessKey: (creds.secretAccessKey || "").trim(),
      sessionToken: (creds.sessionToken || "").trim() || undefined,
      region: (creds.region || "").trim() || undefined,
      roleArn: (creds.roleArn || "").trim() || undefined,
    };
    storage().setItem(STORAGE_KEY, JSON.stringify(clean));
    notifyChanged();
    return clean;
  }

  /**
   * Parse credentials out of a pasted blob. Accepts:
   *  - the STS AssumeRole response JSON (e.g. Isengard "copy credentials"),
   *    which nests them under a top-level `credentials` object;
   *  - a flat JSON object `{ accessKeyId, secretAccessKey, sessionToken?,
   *    region? }` (also tolerates snake_case / SCREAMING_CASE keys).
   *
   * Returns `{ accessKeyId, secretAccessKey, sessionToken?, region? }`.
   * Throws if the required fields can't be found.
   *
   * @param {string} text the pasted blob
   */
  function parse(text) {
    if (!text || !text.trim()) {
      throw new Error("nothing to parse");
    }
    let obj;
    try {
      obj = JSON.parse(text);
    } catch {
      throw new Error("not valid JSON");
    }

    // STS responses nest the credentials under `credentials` (or `Credentials`).
    const c = obj.credentials || obj.Credentials || obj;

    // Accept the common key spellings without inventing values.
    const pick = (...names) => {
      for (const n of names) {
        if (c[n] != null && c[n] !== "") return String(c[n]);
      }
      return undefined;
    };
    const accessKeyId = pick("accessKeyId", "AccessKeyId", "aws_access_key_id");
    const secretAccessKey = pick(
      "secretAccessKey",
      "SecretAccessKey",
      "aws_secret_access_key"
    );
    const sessionToken = pick(
      "sessionToken",
      "SessionToken",
      "aws_session_token",
      "token"
    );
    // Region may live alongside the creds or at the top level.
    const region =
      pick("region", "Region", "aws_region") ||
      (obj.region || obj.Region || undefined);

    if (!accessKeyId || !secretAccessKey) {
      throw new Error(
        "could not find accessKeyId / secretAccessKey in pasted JSON"
      );
    }
    return { accessKeyId, secretAccessKey, sessionToken, region };
  }

  /**
   * Syntactic check that `arn` names a single IAM role, mirroring the server's
   * `is_valid_role_arn` (src/server/credentials.rs) so the UI rejects a
   * malformed value up front instead of round-tripping to a 400. Shape:
   * `arn:{aws|aws-cn|aws-us-gov}:iam::{12-digit account}:role/{name}` (or
   * `role/{path}/{name}`), name non-empty and wildcard-free.
   */
  function isValidRoleArn(arn) {
    if (!arn || arn.length > 2048) return false;
    // arn : partition : service : region : account : resource
    const parts = arn.split(":");
    if (parts.length < 6) return false;
    const [prefix, partition, service, region, account] = parts;
    const resource = parts.slice(5).join(":");
    if (prefix !== "arn") return false;
    if (!["aws", "aws-cn", "aws-us-gov"].includes(partition)) return false;
    if (service !== "iam" || region !== "") return false;
    if (!/^[0-9]{12}$/.test(account)) return false;
    if (!resource.startsWith("role/")) return false;
    const rest = resource.slice("role/".length);
    return rest.length > 0 && !rest.includes("*") && !rest.includes("?");
  }

  /**
   * Store an assume-role ARN as the active credential (the linkable
   * `?aws_role_arn=…` path). Clears any static BYOC keys so the two transports
   * never coexist — the server rejects both together (`ConflictingCredentials`).
   * An optional `region` pins the S3 endpoint, exactly like the BYOC region.
   *
   * Throws on a malformed ARN so a bad link fails loudly rather than silently
   * falling back to the server's default identity. Returns the stored object.
   *
   * @param {string} roleArn IAM role ARN the server should assume
   * @param {{region?: string}} [opts]
   */
  function setRoleArn(roleArn, opts = {}) {
    const arn = (roleArn || "").trim();
    if (!isValidRoleArn(arn)) {
      throw new Error(`invalid role ARN: ${roleArn}`);
    }
    return store({ roleArn: arn, region: opts.region });
  }

  /**
   * Set credentials. This is the stable scripting API — a userscript injects
   * credentials with a single call:
   *
   *   await window.Dial9Creds.set({ accessKeyId, secretAccessKey, sessionToken,
   *                                 region, autoDetectRegion: true });
   *
   * When `autoDetectRegion` is true (and no region is given) the credentials are
   * validated against `/api/credentials/check` for the given (or current
   * default) bucket, and the resolved region is stored. Returns a result object
   * `{ ok, region, error }`. In Node (no fetch) the validation step is skipped
   * and the credentials are stored as-is.
   *
   * @param {{accessKeyId, secretAccessKey, sessionToken?, region?,
   *          autoDetectRegion?: boolean, bucket?: string}} creds
   */
  async function set(creds) {
    if (!creds || !creds.accessKeyId || !creds.secretAccessKey) {
      throw new Error("accessKeyId and secretAccessKey are required");
    }

    // Store first so the headers are available to the check request itself.
    store(creds);

    const wantCheck =
      (creds.autoDetectRegion || !creds.region) &&
      typeof fetch !== "undefined";
    if (!wantCheck) {
      return { ok: true, region: creds.region || undefined, error: null };
    }

    try {
      const result = await check(creds.bucket);
      if (result.ok && result.region) {
        // Persist the resolved region for subsequent requests.
        store({ ...get(), region: result.region });
      }
      // Intentionally do NOT clear the stored credentials when the check fails.
      // A failed check is often bucket-specific (wrong bucket name, or no access
      // to *this* bucket) while the credentials are perfectly valid for others —
      // and the UI lets the user pick a different bucket from the picker after
      // Apply. Wiping creds on the first failed bucket check would break that
      // flow. The caller gets `result.ok === false` and decides what to do
      // (re-pick a bucket, or call `clear()`).
      return result;
    } catch (e) {
      return { ok: false, region: null, error: String(e && e.message || e) };
    }
  }

  /**
   * Validate the stored credentials and detect the bucket's region via
   * `POST /api/credentials/check`. Browser-only (needs fetch + the backend).
   *
   * @param {string} [bucket] bucket to check; defaults to the server default
   * @returns {Promise<{ok, region, error}>}
   */
  async function check(bucket) {
    const url =
      "/api/credentials/check" +
      (bucket ? "?bucket=" + encodeURIComponent(bucket) : "");
    const resp = await fetch(url, { method: "POST", headers: headers() });
    if (!resp.ok) {
      const body = await resp.text().catch(() => "");
      return {
        ok: false,
        region: null,
        error: `HTTP ${resp.status}${body ? ": " + body : ""}`,
      };
    }
    return await resp.json();
  }

  /**
   * List the buckets the stored credentials can see (`GET /api/buckets`), so
   * the UI can offer a picker. Browser-only. Throws on HTTP error with the
   * server's message (e.g. credentials rejected).
   *
   * @returns {Promise<string[]>}
   */
  async function listBuckets() {
    const resp = await fetch("/api/buckets", { headers: headers() });
    if (!resp.ok) {
      const body = await resp.text().catch(() => "");
      throw new Error(`HTTP ${resp.status}${body ? ": " + body : ""}`);
    }
    return await resp.json();
  }

  /** Clear stored credentials and notify listeners. */
  function clear() {
    storage().removeItem(STORAGE_KEY);
    notifyChanged();
  }

  /**
   * Build the `x-dial9-aws-*` request headers from the stored credentials.
   * Returns an empty object when nothing is stored (so it can be spread into any
   * fetch unconditionally). Token/region keys are omitted when unset.
   *
   * Two mutually-exclusive transports (the server rejects both at once with
   * `ConflictingCredentials`):
   *  - static BYOC keys → the four `access-key-id`/`secret-access-key`/
   *    `session-token`/`region` headers;
   *  - an assume-role ARN → the single `role-arn` header (+ optional `region`).
   * Static keys win when both happen to be stored, since a full key set is the
   * more specific intent; the region rides along either way.
   */
  function headers() {
    const c = get();
    if (!c) return {};

    if (c.accessKeyId && c.secretAccessKey) {
      const h = {
        [H.accessKeyId]: c.accessKeyId,
        [H.secretAccessKey]: c.secretAccessKey,
      };
      if (c.sessionToken) h[H.sessionToken] = c.sessionToken;
      if (c.region) h[H.region] = c.region;
      return h;
    }

    if (c.roleArn) {
      const h = { [H.roleArn]: c.roleArn };
      if (c.region) h[H.region] = c.region;
      return h;
    }

    return {};
  }

  /** Fire a `dial9:credentials-changed` event so the UI can refresh. */
  function notifyChanged() {
    if (typeof window !== "undefined" && typeof CustomEvent !== "undefined") {
      window.dispatchEvent(new CustomEvent("dial9:credentials-changed"));
    }
  }

  const Dial9Creds = {
    get,
    has,
    set,
    setRoleArn,
    isValidRoleArn,
    parse,
    check,
    listBuckets,
    clear,
    headers,
    // Test seam — inject a fake storage backend.
    _setStorage(s) {
      injectedStorage = s;
    },
  };

  // Browser: expose on window as the stable scripting contract.
  if (typeof window !== "undefined") {
    window.Dial9Creds = Dial9Creds;
  }
  // Node: CommonJS export for tests.
  if (typeof module !== "undefined" && module.exports) {
    module.exports = { Dial9Creds };
  } else if (typeof exports !== "undefined") {
    exports.Dial9Creds = Dial9Creds;
  }
})(typeof exports === "undefined" ? this : exports);
