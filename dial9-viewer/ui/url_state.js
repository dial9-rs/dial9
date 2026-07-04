"use strict";

// url_state.js — serialize/restore the trace-browser landing page state to and
// from the URL query string, so a search is shareable and survives reload.
//
// Shared between index.html (loaded via <script src>) and the unit tests
// (loaded via require). Keep this dependency-free so both contexts can use it.
//
// State shape (all fields optional):
//   { bucket, region, prefix, tab, tz, last, from, to, q }
//     bucket : S3 bucket name (string)
//     region : S3 region the bucket lives in (string) — serialized as
//              `aws_region`. Carried so a cross-region bucket is signed for the
//              right endpoint and a shared link reproduces it (the region is not
//              a secret; the credentials are header-only and never in the URL).
//     prefix : user-entered key prefix (string)
//     tab    : 'browse' | 'raw'   (default 'browse' — omitted from URL)
//     tz     : 'utc' | 'local'    (default 'utc'    — omitted from URL)
//     last   : relative window in hours (number) — a quick range like "Last
//              24hr". Stored relative so a shared link always means "the last N
//              hours from now", not a frozen window.
//     from   : range start, epoch seconds (integer) — a precise/custom window
//     to     : range end,   epoch seconds (integer) — a precise/custom window
//     q      : raw-search prefix query (string)
//
// `last` and `from`/`to` are mutually exclusive: a quick range serializes as
// `last`, a manually-edited range as precise `from`/`to`. Defaults are omitted
// from the query string so a pristine page produces a clean URL.

(function (exports) {
  // Parse a query string (with or without leading "?") into a state object.
  // Only well-formed values are kept; anything missing/invalid is left unset
  // so callers can fall back to their own defaults.
  function parse(search) {
    const p = new URLSearchParams(search || "");
    const out = {};

    const bucket = p.get("bucket");
    if (bucket) out.bucket = bucket;

    // `aws_region` matches the query param the backend already reads for the
    // assume-role path, so one name means the same thing everywhere.
    const region = p.get("aws_region");
    if (region) out.region = region;

    const prefix = p.get("prefix");
    if (prefix) out.prefix = prefix;

    const q = p.get("q");
    if (q) out.q = q;

    const tab = p.get("tab");
    if (tab === "browse" || tab === "raw") out.tab = tab;

    const tz = p.get("tz");
    if (tz === "utc" || tz === "local") out.tz = tz;

    // A relative quick range ("last N hours") takes precedence over a precise
    // window: it's the more shareable intent, and the two are mutually
    // exclusive on write, so a well-formed `last` means there is no from/to.
    const last = toPositiveNumber(p.get("last"));
    if (last != null) {
      out.last = last;
    } else {
      const from = toEpoch(p.get("from"));
      if (from != null) out.from = from;

      const to = toEpoch(p.get("to"));
      if (to != null) out.to = to;
    }

    return out;
  }

  // Serialize a state object into a query string (no leading "?"). Empty and
  // default-valued fields are omitted, and keys are emitted in a stable order
  // so the same state always yields the same string.
  function serialize(state) {
    const s = state || {};
    const p = new URLSearchParams();

    if (s.bucket) p.set("bucket", s.bucket);
    if (s.region) p.set("aws_region", s.region);
    if (s.prefix) p.set("prefix", s.prefix);
    // 'browse' is the default tab, so only the non-default 'raw' is recorded.
    if (s.tab === "raw") p.set("tab", s.tab);
    // 'utc' is the default timezone, so only 'local' is recorded.
    if (s.tz === "local") p.set("tz", s.tz);
    // A relative quick range wins over a precise window so the link keeps
    // meaning "the last N hours from now" instead of freezing the timestamps.
    if (Number.isFinite(s.last) && s.last > 0) {
      p.set("last", String(s.last));
    } else {
      if (Number.isFinite(s.from)) p.set("from", String(s.from));
      if (Number.isFinite(s.to)) p.set("to", String(s.to));
    }
    if (s.q) p.set("q", s.q);

    return p.toString();
  }

  // Parse an epoch-seconds string into an integer, or null if absent/invalid.
  function toEpoch(v) {
    if (v == null || v === "") return null;
    const n = Number(v);
    return Number.isFinite(n) ? Math.trunc(n) : null;
  }

  // Parse a strictly-positive number (used for the relative `last` window), or
  // null if absent/invalid/non-positive.
  function toPositiveNumber(v) {
    if (v == null || v === "") return null;
    const n = Number(v);
    return Number.isFinite(n) && n > 0 ? n : null;
  }

  exports.parse = parse;
  exports.serialize = serialize;

  // Browser: expose on window as the stable scripting contract.
  if (typeof window !== "undefined") {
    window.Dial9UrlState = exports;
  }
})(typeof exports === "undefined" ? {} : exports);
