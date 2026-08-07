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
  if (epochs.length >= 2) {
    params.set("from", formatEpoch(epochs[0]!, opts));
    params.set("to", formatEpoch(epochs[epochs.length - 1]!, opts));
  } else if (epochs.length === 1) {
    params.set("from", formatEpoch(epochs[0]!, opts));
  }
  params.set("segs", String(keys.length));
  return params;
}
