// Cross-page URL projection for the embedded region flamegraph. The viewer
// store supplies the canonical widget fields; the mutable widget is never read
// while constructing application links.

import type { ViewerViewSlice } from "../../types/state.js";
import type { TimeRange } from "../../types/trace.js";

type RegionFlamegraphState = Pick<
  ViewerViewSlice,
  "regionWorkerZoom" | "regionOffworkerZoom" | "regionInspectFocus"
>;

/**
 * Build the exact-mode flamegraph pop-out URL, or null when the current source
 * has no repeatable `trace=` URL components.
 */
export function buildRegionFlamegraphPopoutUrl(
  search: string,
  range: TimeRange,
  canonical: RegionFlamegraphState,
  sourceShareable: boolean,
): string | null {
  if (!sourceShareable) return null;
  const source = new URLSearchParams(search);
  const traceUrls = source.getAll("trace").filter((url) => url.length > 0);
  if (traceUrls.length === 0) return null;

  const params = new URLSearchParams();
  for (const url of traceUrls) params.append("trace", url);
  params.set("start", String(Math.round(range.startNs)));
  params.set("end", String(Math.round(range.endNs)));
  if (canonical.regionWorkerZoom.length > 0) {
    params.set("worker-zoom", canonical.regionWorkerZoom.join("\t"));
  }
  if (canonical.regionOffworkerZoom.length > 0) {
    params.set("offworker-zoom", canonical.regionOffworkerZoom.join("\t"));
  }
  if (canonical.regionInspectFocus !== null) {
    params.set("inspect", canonical.regionInspectFocus);
  }
  return `flamegraph.html?${params.toString()}`;
}
