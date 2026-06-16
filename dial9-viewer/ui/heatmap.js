"use strict";

// Pure data helpers for the S3 browser density timeline / heatmap.
//
// The browse view only has S3 object-listing metadata available (key, size,
// last_modified) plus the trace-start epoch parsed from the key. We therefore
// approximate "data density" by spreading each segment's byte size uniformly
// across the wall-clock interval it covers ([start, end]); summing those
// contributions per pixel column yields a bytes-per-time signal that makes a
// rotation spike (e.g. one minute with 50 MB while the rest have ~1 MB) stand
// out clearly. Real per-event density would require downloading and decoding
// every segment, which is not viable for a browse listing.
//
// A segment's end is `last_modified` (S3 upload time), which runs a second or
// two past the next segment's start because of upload lag. Left as-is, those
// trailing bytes get counted in BOTH segments at the seam — double-counting
// density into a bright boundary artifact. tileSegments() clamps each end to
// the next start so consecutive segments tile instead of overlap; the leftover
// holes (genuine missing coverage) are surfaced by segmentGaps().
//
// All functions here are pure and unit-tested in test_heatmap.js. Rendering and
// pointer interaction live in index.html and call into these helpers.

// Maximum total bytes we allow opening in the trace viewer at once. Opening a
// very wide time range would download hundreds of MB and overwhelm the viewer,
// so we cap it and nudge the user toward narrow (1–2 minute) selections.
const MAX_OPEN_BYTES = 100 * 1024 * 1024;

// Minimum duration (seconds) attributed to a segment whose end time is unknown
// or not strictly after its start, so a zero-width segment still renders as a
// localized spike instead of vanishing.
const MIN_SEGMENT_SECONDS = 1;

// Normalize a segment's [start, end] span in seconds, guaranteeing end > start.
function segmentSpan(seg) {
    const start = seg.start;
    let end = seg.end;
    if (!(end > start)) end = start + MIN_SEGMENT_SECONDS;
    return { start, end };
}

// Group normalized segments into one row per service/host. A change in boot_id
// does NOT create a separate row — boot transitions are surfaced as in-row
// markers via bootTransitions(). Returns rows sorted by label, each with its
// segments sorted by start time.
//
// A normalized segment is: { key, size, start, end, service, host, bootId }
// where start/end are epoch seconds.
function groupByHost(segments) {
    const rows = new Map();
    for (const seg of segments) {
        const key = (seg.service || "") + "\u0000" + (seg.host || "");
        let row = rows.get(key);
        if (!row) {
            row = {
                service: seg.service || "",
                host: seg.host || "",
                label: (seg.service || "") + " / " + (seg.host || ""),
                segments: [],
                totalBytes: 0,
            };
            rows.set(key, row);
        }
        row.segments.push(seg);
        row.totalBytes += seg.size || 0;
    }
    const out = [...rows.values()];
    for (const row of out) row.segments.sort((a, b) => a.start - b.start);
    out.sort((a, b) => a.label.localeCompare(b.label));
    return out;
}

// Detect boot_id transitions within a row's segments (which need not be
// pre-sorted; a sorted copy is used). Returns [{ time, fromBoot, toBoot }] at
// the start of each segment whose boot id differs from the previous non-empty
// boot id. Segments without a boot id (legacy layout) produce no transitions.
function bootTransitions(rowSegments) {
    const sorted = [...rowSegments].sort((a, b) => a.start - b.start);
    const out = [];
    let prev = null;
    for (const seg of sorted) {
        const boot = seg.bootId || "";
        if (!boot) continue;
        if (prev !== null && boot !== prev) {
            out.push({ time: seg.start, fromBoot: prev, toBoot: boot });
        }
        prev = boot;
    }
    return out;
}

// Accumulate bytes-per-pixel-column for a row over [t0, t1] (seconds) into a
// Float64Array of length `width`. Each segment's bytes are spread uniformly in
// time across its [start, end] span; each pixel column receives the portion of
// bytes whose time falls within that column's sub-interval. Columns all share
// the same time width, so the resulting values are directly comparable for
// color normalization.
function accumulateDensity(segments, t0, t1, width) {
    const cols = new Float64Array(Math.max(0, width));
    if (!(t1 > t0) || width <= 0) return cols;
    const secPerCol = (t1 - t0) / width;
    for (const seg of segments) {
        const { start, end } = segmentSpan(seg);
        const span = end - start; // > 0 by construction
        const rate = (seg.size || 0) / span; // bytes per second
        const cs = Math.max(start, t0);
        const ce = Math.min(end, t1);
        if (!(ce > cs)) continue;
        let c0 = Math.floor((cs - t0) / secPerCol);
        let c1 = Math.floor((ce - t0) / secPerCol);
        if (c0 < 0) c0 = 0;
        if (c1 >= width) c1 = width - 1;
        for (let c = c0; c <= c1; c++) {
            const colStart = t0 + c * secPerCol;
            const colEnd = colStart + secPerCol;
            const ov = Math.min(ce, colEnd) - Math.max(cs, colStart);
            if (ov > 0) cols[c] += rate * ov; // bytes attributable to this column
        }
    }
    return cols;
}

// Return density-rendering copies of a row's segments with each segment's end
// clamped to the next segment's start, so consecutive segments tile instead of
// overlapping at the seam (see the file header for why ends overshoot). Input
// need not be sorted; a sorted copy drives the clamp. The original `end` is
// preserved on `realEnd` so callers that need the true file extent (selection,
// gap detection) are unaffected. Bytes are NOT rescaled: clamping shortens the
// span so the same bytes spread over slightly less time — a small, uniform
// density bump that is far less misleading than the double-count it removes.
function tileSegments(rowSegments) {
    const sorted = [...rowSegments].sort((a, b) => a.start - b.start);
    return sorted.map((seg, i) => {
        const next = sorted[i + 1];
        let end = seg.end;
        // Clamp only on real overlap, and never past the start (segmentSpan
        // gives a zero/negative span its MIN_SEGMENT_SECONDS floor).
        if (next && next.start > seg.start && next.start < end) end = next.start;
        return { ...seg, end, realEnd: seg.end };
    });
}

// Genuine coverage gaps within a row: intervals [end_i, start_{i+1}] where the
// next segment starts after the current one's real end. Normal back-to-back
// rotation overlaps (upload lag) so it yields no gap; only real missing
// coverage (a host that stopped reporting for a while) does. Input need not be
// sorted. Returns [{ start, end }] in seconds, sorted by start.
function segmentGaps(rowSegments) {
    const sorted = [...rowSegments].sort((a, b) => a.start - b.start);
    const out = [];
    for (let i = 0; i < sorted.length - 1; i++) {
        const end = segmentSpan(sorted[i]).end;
        const nextStart = sorted[i + 1].start;
        if (nextStart > end) out.push({ start: end, end: nextStart });
    }
    return out;
}

// Segments whose [start, end) span touches the query range [t0, t1] in seconds.
// The start is inclusive so a point query (t0 === t1, used for a single click)
// landing exactly on a segment's start still selects it; the end stays
// exclusive so a click on the boundary between two adjacent segments picks the
// later one (the segment that starts there) rather than both.
function segmentsOverlapping(segments, t0, t1) {
    return segments.filter((seg) => {
        const { start, end } = segmentSpan(seg);
        return start <= t1 && end > t0;
    });
}

// Total byte size of a set of segments (used for the open-size cap).
function totalBytes(segments) {
    return segments.reduce((sum, seg) => sum + (seg.size || 0), 0);
}

// Map a normalized density value in [0, 1] to a CSS color. A value of 0 (no
// data) returns the page background; positive values ramp dim-blue → purple →
// red → yellow. A perceptual sqrt curve keeps a low baseline visible while
// letting spikes saturate toward the bright end so they are easy to spot.
function densityColor(norm) {
    if (!(norm > 0)) return "rgb(26,26,46)"; // #1a1a2e page background
    const t = Math.sqrt(Math.min(norm, 1));
    const stops = [
        [0.0, 42, 42, 90], // dim blue
        [0.35, 108, 99, 255], // #6c63ff purple (theme accent)
        [0.7, 255, 107, 107], // #ff6b6b red
        [1.0, 255, 217, 61], // #ffd93d yellow
    ];
    for (let i = 1; i < stops.length; i++) {
        if (t <= stops[i][0]) {
            const a = stops[i - 1];
            const b = stops[i];
            const f = (t - a[0]) / (b[0] - a[0]);
            const r = Math.round(a[1] + (b[1] - a[1]) * f);
            const g = Math.round(a[2] + (b[2] - a[2]) * f);
            const bl = Math.round(a[3] + (b[3] - a[3]) * f);
            return `rgb(${r},${g},${bl})`;
        }
    }
    return "rgb(255,217,61)";
}

if (typeof module !== "undefined" && module.exports) {
    module.exports = {
        MAX_OPEN_BYTES,
        MIN_SEGMENT_SECONDS,
        segmentSpan,
        groupByHost,
        bootTransitions,
        tileSegments,
        segmentGaps,
        accumulateDensity,
        segmentsOverlapping,
        totalBytes,
        densityColor,
    };
}
