// Trace title metadata, rendered in the viewer and flamegraph headers.

import { formatEpoch, parseKey, type EpochFormatOptions } from "./keys.js";

/**
 * Structured title metadata (svc / host / time window / segment count)
 * derived from a set of selected keys, as URL query params:
 *
 * - `svc`: unique services, ", "-joined (omitted when none parse).
 * - `host`: set ONLY when every parsed key agrees on a single host;
 *   multi-host selections drop it.
 * - `from`/`to`: formatted min/max segment epoch; `from` alone when only
 *   one distinct epoch exists.
 * - `segs`: total number of selected keys, always set.
 *
 * Unknown-layout keys (see keys.ts) carry no service/host, so they never
 * contribute to `svc`/`host` - the deliberate consequence of the parser's
 * defect fix. Their FILENAME epoch is layout-independent, though, so it
 * still contributes to the `from`/`to` window.
 */
export function traceTitleParams(
  keys: readonly string[],
  opts: EpochFormatOptions = {}
): URLSearchParams {
  const parsed = keys.map((k) => parseKey(k));
  const known = parsed.filter((p) => p.layout === "known");
  const services = [...new Set(known.map((p) => p.service).filter(Boolean))];
  const hosts = [...new Set(known.map((p) => p.host).filter(Boolean))];
  const epochs = parsed
    .map((p) => p.epoch)
    .filter((e) => e > 0)
    .sort((a, b) => a - b);
  const params = new URLSearchParams();
  if (services.length) params.set("svc", services.join(", "));
  if (hosts.length === 1) params.set("host", hosts[0]!);
  const stamp = { ...opts, withZone: true };
  if (epochs.length >= 2) {
    params.set("from", formatEpoch(epochs[0]!, stamp));
    params.set("to", formatEpoch(epochs[epochs.length - 1]!, stamp));
  } else if (epochs.length === 1) {
    params.set("from", formatEpoch(epochs[0]!, stamp));
  }
  params.set("segs", String(keys.length));
  return params;
}

/**
 * Browser tab title for a page: `"<service> | <page>"` when the service is
 * known, otherwise just `<page>`. Leading/trailing whitespace on the service
 * is ignored, and a blank service counts as unknown.
 */
export function pageTitle(page: string, service?: string | null): string {
  const svc = service?.trim();
  return svc ? `${svc} | ${page}` : page;
}

/**
 * Service label for a two-sided diff page: the shared service when both sides
 * agree, `"a vs b"` when they differ (`?` stands in for a side with no
 * service), and null when neither side has one.
 */
export function diffServiceLabel(
  a?: string | null,
  b?: string | null,
): string | null {
  const sa = a?.trim() ?? "";
  const sb = b?.trim() ?? "";
  if (!sa && !sb) return null;
  if (sa === sb) return sa;
  return `${sa || "?"} vs ${sb || "?"}`;
}

/**
 * Host-count suffix for a tab title's page name: `" @ (1 host)"`,
 * `" @ (5 hosts)"`. Empty when the count is unknown (null) or not positive,
 * so an uncertain count is never shown.
 */
export function hostCountSuffix(hosts: number | null | undefined): string {
  if (hosts == null || !(hosts > 0)) return "";
  return ` @ (${hosts} ${hosts === 1 ? "host" : "hosts"})`;
}

/**
 * Count the distinct hosts behind a set of `trace=` URLs. Each URL's S3 key
 * comes from its `key` query param (`/api/object?...&key=`) or, failing that,
 * its path. Returns null when the list is empty or ANY URL lacks a key in a
 * recognized layout (see keys.ts): a partial count would understate it.
 */
export function countTraceHosts(urls: readonly string[]): number | null {
  if (urls.length === 0) return null;
  const hosts = new Set<string>();
  for (const url of urls) {
    let key: string;
    try {
      const u = new URL(url, "http://localhost");
      key = u.searchParams.get("key") ?? u.pathname.replace(/^\//, "");
    } catch {
      return null;
    }
    const parsed = parseKey(key);
    if (parsed.layout !== "known" || !parsed.host) return null;
    hosts.add(parsed.host);
  }
  return hosts.size;
}
