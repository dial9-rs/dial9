// The viewer's color vocabulary as typed utilities.
//
// - Frozen-core palettes re-exported: pollHeatmapColor /
//   pollHeatmapColorQuantized (poll-duration heatmap) and flamegraphColor
//   (deterministic warm frame color) live in trace_analysis.js.
// - App-level color logic: pollColor (uses the quantized heatmap so
//   equal-ish durations share a color string and the bar coalescer can
//   merge them), the memoized dimmer, and the stable name->color assigner.

// MUST precede the trace_analysis.js import: its factory reads the
// TraceParser browser global in bundled entries (see core-globals.ts).
import "./core-globals.js";
import { pollHeatmapColorQuantized } from "../../../trace_analysis.js";

export {
  flamegraphColor,
  pollHeatmapColor,
  pollHeatmapColorQuantized,
} from "../../../trace_analysis.js";

/**
 * Duration-based poll coloring: short=dim, long=hot. Quantized variant
 * (<= 24 distinct strings) so adjacent approximately-equal polls share a
 * color and the LOD path merges them into one fillRect.
 */
export function pollColor(startNs: number, endNs: number): string {
  return pollHeatmapColorQuantized(endNs - startNs);
}

/**
 * A memoized channel-multiply dimmer: `#rrggbb` -> `#rrggbb` with each
 * channel scaled by `factor` (hue preserved, bar visually recedes). The
 * transform depends only on the (quantized, <= 24 distinct) input color,
 * and it runs once per NON-selected poll per render - millions of times on
 * a large trace - so it must collapse to a Map lookup after the first hit.
 *
 * Throws on non-`#rrggbb` input and factors outside [0, 1] rather than
 * emitting a plausible-looking wrong color.
 */
export function makeColorDimmer(factor: number = 0.4): (color: string) => string {
  if (!(factor >= 0 && factor <= 1)) {
    throw new RangeError(
      `makeColorDimmer: factor must be in [0, 1], got ${factor}`,
    );
  }
  const cache = new Map<string, string>();
  return (color) => {
    let dim = cache.get(color);
    if (dim === undefined) {
      if (!/^#[0-9a-fA-F]{6}$/.test(color)) {
        throw new RangeError(
          `makeColorDimmer: expected "#rrggbb", got "${color}"`,
        );
      }
      const ch = (i: number) =>
        Math.round(parseInt(color.slice(i, i + 2), 16) * factor)
          .toString(16)
          .padStart(2, "0");
      dim = `#${ch(1)}${ch(3)}${ch(5)}`;
      cache.set(color, dim);
    }
    return dim;
  };
}

/**
 * The categorical palette used for tracing spans AND custom events (one
 * list, two assignment maps).
 */
export const SPAN_COLORS: readonly string[] = [
  "#7c4dff", "#00bfa5", "#ff6e40", "#448aff", "#c6ff00",
  "#ea80fc", "#ff5252", "#64ffda", "#ffd740", "#b388ff",
  "#69f0ae", "#ff80ab", "#40c4ff", "#ffab40", "#b2ff59",
  "#8c9eff", "#f48fb1", "#80cbc4", "#ffe57f", "#ce93d8",
];

/**
 * Stable name->color assignment: first-seen order walks the palette,
 * wrapping past the end; a given name keeps its color for the assigner's
 * lifetime. Build one assigner per namespace so span names and event names
 * get independent sequences.
 */
export function makeColorAssigner(
  palette: readonly string[] = SPAN_COLORS,
): (name: string) => string {
  if (palette.length === 0) {
    throw new RangeError("makeColorAssigner: palette must not be empty");
  }
  const assigned = new Map<string, string>();
  return (name) => {
    let color = assigned.get(name);
    if (color === undefined) {
      color = palette[assigned.size % palette.length]!;
      assigned.set(name, color);
    }
    return color;
  };
}
