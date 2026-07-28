// Boundary polls: the truncation hard edge between adjacent segments.
//
// A poll that starts in one segment and ends in the next is real, but neither
// segment alone can see it. This module reconstructs that evidence -- what was
// open at a segment's end, what closed at the next one's start -- and stitches
// the pairs into whole polls, so a windowed view reports a poll's true extent
// instead of one clipped at the window edge.
//
// Absence is never fabricated: an ambiguous edge (a segment starting on
// Park/Unpark rather than PollEnd) is captured as NOTHING, and a stitched poll
// that only ever had one side is reported as a lower bound.
//
// Split out of segments.ts, which owns the caching/eviction orchestration.
// This is a separate domain: pure functions over parsed events, with no
// knowledge of budgets, residency or the worker.

import { EVENT_TYPES } from "../../../trace_parser.js";
import type { ParsedTrace, TraceEvent } from "../../../trace_parser.js";
import type {
  SegmentEdgeDanglingClose,
  SegmentEdgeOpenPoll,
  SegmentEdgePolls,
  StitchedBoundaryPoll,
  TimeRange,
  WindowBoundaryPolls,
  WindowEdgePoll,
} from "../../types/trace.js";
import type {
  SegmentEntry,
  SegmentParseInvariants,
} from "../../types/state.js";

// ── 5. Boundary polls (truncation hard edge) ─────────────────────────────

/**
 * Extract the boundary-poll evidence at a segment parse's edges in one
 * event scan (see types/trace.d.ts for the semantics):
 *
 * - openAtEnd: a PollStart with no closing event (PollEnd / WorkerPark /
 *   next PollStart) before the segment ends - exactly the polls the
 *   frozen core DISCARDS at trace end.
 * - closeAtStart: the worker's first lifecycle event (PollStart, PollEnd,
 *   WorkerPark or WorkerUnpark) is a PollEnd - the poll began before this
 *   segment. A Park/Unpark-first segment start is ambiguous and captured
 *   as NOTHING (absence is never fabricated).
 */
export function computeSegmentEdgePolls(
  trace: Pick<ParsedTrace, "events">
): SegmentEdgePolls {
  const perWorker = new Map<number, TraceEvent[]>();
  for (const e of trace.events) {
    if (
      e.eventType === EVENT_TYPES.PollStart ||
      e.eventType === EVENT_TYPES.PollEnd ||
      e.eventType === EVENT_TYPES.WorkerPark ||
      e.eventType === EVENT_TYPES.WorkerUnpark
    ) {
      let list = perWorker.get(e.workerId);
      if (list === undefined) {
        list = [];
        perWorker.set(e.workerId, list);
      }
      list.push(e);
    }
  }
  const openAtEnd: SegmentEdgeOpenPoll[] = [];
  const closeAtStart: SegmentEdgeDanglingClose[] = [];
  for (const [workerId, events] of perWorker) {
    events.sort((a, b) => a.timestamp - b.timestamp);
    const first = events[0]!;
    if (first.eventType === EVENT_TYPES.PollEnd) {
      closeAtStart.push({ workerId, end: first.timestamp });
    }
    let open: TraceEvent | null = null;
    for (const e of events) {
      if (e.eventType === EVENT_TYPES.PollStart) {
        open = e;
      } else if (
        e.eventType === EVENT_TYPES.PollEnd ||
        e.eventType === EVENT_TYPES.WorkerPark
      ) {
        // Park closes an open poll WITHIN the segment (block_in_place
        // handoff, already openEnded via the core) - not a boundary case.
        open = null;
      }
    }
    if (open !== null) {
      openAtEnd.push({
        workerId,
        start: open.timestamp,
        taskId: open.taskId,
        spawnLocId: open.spawnLocId,
        spawnLoc: open.spawnLoc,
      });
    }
  }
  openAtEnd.sort((a, b) => a.workerId - b.workerId);
  closeAtStart.sort((a, b) => a.workerId - b.workerId);
  return { openAtEnd, closeAtStart };
}

/**
 * Parse-derived invariants retained across eviction (lanes/axes stay
 * stable). Worker ids come from the poll/park lifecycle events - the same
 * events that create lanes.
 */
export function segmentInvariants(
  trace: Pick<ParsedTrace, "events" | "minTs" | "maxTs">
): SegmentParseInvariants {
  const workers = new Set<number>();
  for (const e of trace.events) {
    if (
      e.eventType === EVENT_TYPES.PollStart ||
      e.eventType === EVENT_TYPES.PollEnd ||
      e.eventType === EVENT_TYPES.WorkerPark ||
      e.eventType === EVENT_TYPES.WorkerUnpark
    ) {
      workers.add(e.workerId);
    }
  }
  return {
    minTs: trace.minTs,
    maxTs: trace.maxTs,
    workerIds: [...workers].sort((a, b) => a - b),
  };
}

/**
 * Derive the window-level boundary-poll view from the segments slice.
 *
 * A poll's lifecycle can span ANY number of segments - segment rotation is
 * not poll-aligned, and a worker blocked in one poll for minutes (the
 * pathology this tool diagnoses) is silent for whole segments. The
 * derivation walks each worker's evidence chain across the listing:
 *
 * - CONTINUITY: an open poll at a segment's end continues through the
 *   next listed segment when (a) their extents are ADJACENT - no
 *   unobserved time between them; listing order alone can hide holes,
 *   and stitching across one would fabricate a completed long poll out
 *   of two different polls' evidence - and (b) the worker is SILENT
 *   there: any PollEnd / WorkerPark / PollStart would have closed the
 *   poll, and silence is proven via the parse invariants' worker set
 *   (which, like edgePolls, survives eviction).
 * - STITCH: a chain whose open and close endpoints and every interior
 *   segment are RESIDENT assembles into ONE complete poll, whether it
 *   crosses one boundary or many.
 * - TRUNCATE: a chain interrupted by anything non-resident (unfetched or
 *   evicted neighbor, extent gap, evidence without retained edges)
 *   surfaces each maximal resident stretch it provably spans as a
 *   WindowEdgePoll truncated at "start"/"end"/"both", spans clamped to
 *   RESIDENT observed events - a lower bound, never an inflated long
 *   poll. Task identity carries whenever the PollStart was ever parsed
 *   (resident or retained edge evidence); otherwise it is explicitly null.
 * - DROP: a chain meeting a boundary where the worker is ACTIVE with
 *   retained edges but no matching counterpart drops whole - the
 *   counterpart never made it to the wire, and the silence proof is only
 *   as good as the wire. Chains running off the ABSOLUTE listing ends
 *   also drop: the frozen core's trace-edge behavior, preserved for parity.
 *
 * Pure derivation over (listing order, entries): recompute when the set of
 * parsed segments changes.
 */
export function computeWindowBoundaryPolls(
  orderedKeys: readonly string[],
  entries: ReadonlyMap<string, SegmentEntry>,
): WindowBoundaryPolls {
  const truncated: WindowEdgePoll[] = [];
  const stitched: StitchedBoundaryPoll[] = [];

  interface Spot {
    entry: SegmentEntry | undefined;
    /** Parsed data present: contributes rendered spans and stitch ends. */
    resident: boolean;
    /** Parse-derived evidence; survives eviction (worker set, bounds). */
    invariants: SegmentParseInvariants | undefined;
    /** Edge evidence; survives eviction (absent on pre-parse entries). */
    edgePolls: SegmentEdgePolls | undefined;
  }
  const spots: Spot[] = orderedKeys.map((key) => {
    const entry = entries.get(key);
    return {
      entry,
      resident:
        entry !== undefined &&
        entry.state === "parsed" &&
        entry.trace !== undefined &&
        entry.edgePolls !== undefined &&
        entry.invariants !== undefined,
      invariants: entry?.invariants,
      edgePolls: entry?.edgePolls,
    };
  });

  /** No unobserved time between two consecutively listed segments. */
  const adjacent = (left: Spot, right: Spot): boolean =>
    left.entry !== undefined &&
    right.entry !== undefined &&
    left.entry.extent.endNs >= right.entry.extent.startNs;

  const silentFor = (spot: Spot, workerId: number): boolean =>
    spot.invariants !== undefined &&
    !spot.invariants.workerIds.includes(workerId);

  // Workers with edge evidence anywhere; others cannot have chains.
  const workers = new Set<number>();
  for (const spot of spots) {
    if (spot.edgePolls === undefined) continue;
    for (const o of spot.edgePolls.openAtEnd) workers.add(o.workerId);
    for (const c of spot.edgePolls.closeAtStart) workers.add(c.workerId);
  }

  /** One maximal RESIDENT span a chain provably covers. */
  interface Stretch {
    start: number;
    end: number;
    /** The chain's PollStart lies inside this stretch (resident). */
    openObserved: boolean;
    /** The chain's PollEnd lies inside this stretch (resident). */
    closeObserved: boolean;
  }

  const emitStretches = (
    workerId: number,
    stretches: readonly Stretch[],
    identity: SegmentEdgeOpenPoll | null
  ): void => {
    for (const s of stretches) {
      truncated.push({
        workerId,
        start: s.start,
        end: s.end,
        taskId: identity !== null ? identity.taskId : null,
        spawnLocId: identity !== null ? identity.spawnLocId : null,
        spawnLoc: identity !== null ? identity.spawnLoc : null,
        truncatedAt: s.openObserved ? "end" : s.closeObserved ? "start" : "both",
        openEnded: true,
      });
    }
  };

  for (const workerId of [...workers].sort((a, b) => a - b)) {
    /** An open poll carried rightward across segment boundaries. */
    interface Carry {
      open: SegmentEdgeOpenPoll;
      /** Origin and every walked segment resident so far. */
      stitchable: boolean;
      stretches: Stretch[];
      /** The last stretch reaches the current boundary (contiguous). */
      live: boolean;
    }
    let carry: Carry | null = null;

    const openAt = (spot: Spot): SegmentEdgeOpenPoll | undefined =>
      spot.edgePolls?.openAtEnd.find((o) => o.workerId === workerId);
    const closeAt = (spot: Spot): SegmentEdgeDanglingClose | undefined =>
      spot.edgePolls?.closeAtStart.find((c) => c.workerId === workerId);

    const startCarry = (spot: Spot, open: SegmentEdgeOpenPoll): Carry => {
      if (spot.resident) {
        const maxTs = spot.invariants!.maxTs;
        return {
          open,
          stitchable: true,
          stretches: [
            {
              start: open.start,
              end: Math.max(maxTs ?? open.start, open.start),
              openObserved: true,
              closeObserved: false,
            },
          ],
          live: true,
        };
      }
      // Evicted neighbor with RETAINED edge evidence: informs continuity
      // and task identity but contributes no rendered span.
      return { open, stitchable: false, stretches: [], live: false };
    };

    /**
     * A dangling close with no live carry: the poll began before this
     * segment - and before every adjacent SILENT segment to its left (a
     * PollStart there would be a lifecycle event). Extend the provable
     * span leftward, then decide the left-edge disposition.
     */
    const resolveDanglingClose = (
      i: number,
      close: SegmentEdgeDanglingClose
    ): void => {
      const spot = spots[i]!;
      const stretchesReversed: Stretch[] = [];
      let live = false;
      if (spot.resident) {
        const minTs = spot.invariants!.minTs;
        stretchesReversed.push({
          start: Math.min(minTs ?? close.end, close.end),
          end: close.end,
          openObserved: false,
          closeObserved: true,
        });
        live = true;
      }
      let reason: "listing-start" | "gap" | "unparsed" | "active" =
        "listing-start";
      let stopper: Spot | null = null;
      for (let j = i - 1; j >= 0; j--) {
        const prev = spots[j]!;
        if (!adjacent(prev, spots[j + 1]!)) {
          reason = "gap";
          break;
        }
        if (prev.invariants === undefined) {
          reason = "unparsed";
          break;
        }
        if (!silentFor(prev, workerId)) {
          reason = "active";
          stopper = prev;
          break;
        }
        if (prev.resident) {
          const { minTs, maxTs } = prev.invariants;
          if (minTs !== null && maxTs !== null) {
            if (live) {
              const last = stretchesReversed[stretchesReversed.length - 1]!;
              last.start = Math.min(last.start, minTs);
            } else {
              stretchesReversed.push({
                start: minTs,
                end: maxTs,
                openObserved: false,
                closeObserved: false,
              });
              live = true;
            }
          }
          // A resident segment with no events keeps contiguity as-is.
        } else {
          live = false; // evidence-only interior: span breaks here
        }
      }
      // Absolute listing start: core trace-edge parity - drop.
      if (reason === "listing-start") return;
      // Active neighbor with retained edges and (necessarily) no
      // matching open - a matching open would have produced a forward
      // carry that consumed this close. Counterpart lost: drop whole.
      if (reason === "active" && stopper!.edgePolls !== undefined) return;
      // Gap, unparsed neighbor, or legacy evidence without retained
      // edges: honestly truncated; identity unknown (no PollStart seen).
      emitStretches(workerId, stretchesReversed.reverse(), null);
    };

    for (let i = 0; i < spots.length; i++) {
      const spot = spots[i]!;
      let closeConsumed = false;

      if (carry !== null) {
        const prev = spots[i - 1]!;
        if (!adjacent(prev, spot)) {
          // Extent gap: the chain breaks - truncated fragments, never a
          // fabricated complete poll.
          emitStretches(workerId, carry.stretches, carry.open);
          carry = null;
        } else if (spot.invariants === undefined) {
          // Never-parsed neighbor (listed/fetching): window edge.
          emitStretches(workerId, carry.stretches, carry.open);
          carry = null;
        } else if (silentFor(spot, workerId)) {
          // The poll provably runs through this WHOLE segment.
          carry.stitchable &&= spot.resident;
          if (spot.resident) {
            const { minTs, maxTs } = spot.invariants;
            if (minTs !== null && maxTs !== null) {
              if (carry.live) {
                const last = carry.stretches[carry.stretches.length - 1]!;
                last.end = Math.max(last.end, maxTs);
              } else {
                carry.stretches.push({
                  start: minTs,
                  end: maxTs,
                  openObserved: false,
                  closeObserved: false,
                });
              }
              carry.live = true;
            }
            // A resident segment with no events keeps contiguity as-is.
          } else {
            carry.live = false; // evidence-only interior: span breaks
          }
          // The carry survives to the next boundary.
        } else if (spot.edgePolls === undefined) {
          // Active worker, evidence without retained edges (entries
          // evicted before edge retention existed): shape unknown -
          // surface what is provable, honestly truncated.
          emitStretches(workerId, carry.stretches, carry.open);
          carry = null;
        } else {
          const close = closeAt(spot);
          if (close === undefined) {
            // Unmatched: the counterpart never made it to the wire.
            // Core trace-edge parity - the whole chain drops.
            carry = null;
          } else if (spot.resident) {
            closeConsumed = true;
            if (carry.stitchable) {
              stitched.push({
                workerId,
                start: carry.open.start,
                end: close.end,
                taskId: carry.open.taskId,
                spawnLocId: carry.open.spawnLocId,
                spawnLoc: carry.open.spawnLoc,
              });
            } else {
              // The close is resident but part of the chain is not: the
              // close-bearing stretch ends AT the observed close.
              if (carry.live) {
                const last = carry.stretches[carry.stretches.length - 1]!;
                last.end = Math.max(last.end, close.end);
                last.closeObserved = true;
              } else {
                const minTs = spot.invariants.minTs;
                carry.stretches.push({
                  start: Math.min(minTs ?? close.end, close.end),
                  end: close.end,
                  openObserved: false,
                  closeObserved: true,
                });
              }
              emitStretches(workerId, carry.stretches, carry.open);
            }
            carry = null;
          } else {
            // Close observed once but its data is not resident
            // (evicted): rendered spans end at the last resident edge.
            closeConsumed = true;
            emitStretches(workerId, carry.stretches, carry.open);
            carry = null;
          }
        }
      }

      if (carry === null) {
        if (!closeConsumed) {
          const close = closeAt(spot);
          if (close !== undefined) resolveDanglingClose(i, close);
        }
        const open = openAt(spot);
        if (open !== undefined) carry = startCarry(spot, open);
      }
    }
    // A carry running off the absolute listing end: core trace-edge
    // parity - dropped, no marker.
  }
  return { truncated, stitched };
}
