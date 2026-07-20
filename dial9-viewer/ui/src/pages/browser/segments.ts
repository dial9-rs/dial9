// The S3-object -> heatmap model pipeline: the pure transforms that turn a
// GET /api/browse listing into the segments, host rows and time extent the
// browse view paints. Plain data in, plain data out - no store, no DOM and
// no fetch; actions.ts owns those and calls through here.

// Leaf seam modules, NOT the lib barrels: the barrel indexes evaluate
// modules that import trace_analysis.js / trace_parser.js at init (they
// expect <script>-established globals), which this page must not load.
import {
  groupByHost,
  segmentGaps,
  segmentSpan,
  tileSegments,
} from "../../lib/canvas/heatmap.js";
import { parseKey } from "../../lib/trace/keys.js";
import { epochSeconds } from "./format.js";
import type {
  BrowseObject,
  HeatmapRow,
  HeatmapSegment,
  TimeDomain,
} from "./state.js";

/**
 * Heatmap grouping key for an unknown-layout S3 key: the key's raw
 * directory path, so segments from the same directory share a row without
 * any guessed service/host labels. Falls back to the whole key for keys
 * with no directory.
 */
export function unknownGroupPath(key: string): string {
  const dir = key.split("/").slice(0, -1).join("/");
  return dir || key;
}

/**
 * Build normalized segments for the density timeline. A segment's
 * wall-clock span is [trace-start epoch, last_modified]; bytes are spread
 * uniformly across it when rendering density. Unknown-layout keys group by
 * their raw directory path; their filename epoch is layout-independent, so
 * time placement is unchanged.
 *
 * Objects with no derivable start (no filename epoch and no upload mtime)
 * are dropped: they cannot be placed on the timeline.
 */
export function toSegments(objects: readonly BrowseObject[]): HeatmapSegment[] {
  return objects
    .map((obj) => {
      const p = parseKey(obj.key);
      // Local traces carry no date/epoch in the key, so fall back to the
      // upload mtime for time placement (#627); S3 traces keep p.epoch.
      const mtime = epochSeconds(obj.last_modified);
      const start = p.epoch || mtime;
      const end = mtime || start;
      const shared = { key: obj.key, size: obj.size, start, end };
      if (p.layout === "known") {
        return {
          ...shared,
          layout: "known" as const,
          service: p.service,
          host: p.host,
          bootId: p.bootId,
        };
      }
      return {
        ...shared,
        layout: "unknown" as const,
        service: "",
        host: unknownGroupPath(obj.key),
        bootId: "",
      };
    })
    .filter((s) => s.start > 0);
}

/**
 * Group segments into host rows, precomputing the per-row density inputs
 * once (they are reused across zoom/resize redraws): segments tiled so
 * upload-lag overlaps don't double-count, plus the genuine coverage gaps
 * between them.
 */
export function toRows(segments: readonly HeatmapSegment[]): HeatmapRow[] {
  return groupByHost(segments).map((row) => ({
    ...row,
    tiled: tileSegments(row.segments),
    gaps: segmentGaps(row.segments),
  }));
}

/** Full data extent across segments. */
export function computeExtent(segments: readonly HeatmapSegment[]): TimeDomain {
  let tMin = Infinity;
  let tMax = -Infinity;
  for (const s of segments) {
    const span = segmentSpan(s);
    if (span.start < tMin) tMin = span.start;
    if (span.end > tMax) tMax = span.end;
  }
  if (!(tMax > tMin)) tMax = tMin + 1;
  return { tMin, tMax };
}
