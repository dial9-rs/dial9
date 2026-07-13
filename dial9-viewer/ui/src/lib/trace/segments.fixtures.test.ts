// The committed synthetic segments under parity/fixtures/segments/ parsed
// through the real core, re-deriving the facts the generator planted in
// manifest.json:
//
//   - a poll spanning the seg0/seg1 boundary (open PollStart at the end of
//     seg0, dangling PollEnd at the start of seg1);
//   - a poll spanning seg3 -> seg5 with the worker completely SILENT through
//     seg4;
//   - a multi-runtime segment whose `runtime.<name>` segment-metadata entry
//     groups workers 64..67 as a named runtime.
//
// Regenerate with:
//   cargo run --release -p dial9-viewer --features dev-server --bin gen-fixtures
// (deterministic: regenerating without a generator change is a no-op diff).

import { readFileSync } from "node:fs";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";
import { gunzipSync } from "node:zlib";
import { beforeAll, describe, expect, it } from "vitest";
import { parseTraceBuffer } from "./load.js";
import {
  computeSegmentEdgePolls,
  computeWindowBoundaryPolls,
  segmentInvariants,
} from "./segments.js";
import type { ParsedTrace } from "../../../trace_parser.js";
import type { SegmentEntry } from "../../types/state.js";

const require = createRequire(import.meta.url);

interface ManifestSegment {
  file: string;
  key: string;
  epochS: number;
  monoStartNs: number;
  monoEndNs: number;
}

interface ManifestPoll {
  workerId: number;
  taskId: number;
  spawnLoc: string;
  startSegment: number;
  startNs: number;
  endSegment: number;
  endNs: number;
}

interface Manifest {
  workers: number[];
  epochBaseS: number;
  segmentSeconds: number;
  monoBaseNs: number;
  clockOffsetNs: string;
  segments: ManifestSegment[];
  plantedPolls: ManifestPoll[];
  multiRuntime: {
    file: string;
    runtimes: Record<string, number[]>;
    mainWorkers: number[];
  };
}

const fixturesDir = new URL("../../../parity/fixtures/segments/", import.meta.url);

function readFixture(name: string): Uint8Array {
  return new Uint8Array(gunzipSync(readFileSync(fileURLToPath(new URL(name, fixturesDir)))));
}

const manifest = JSON.parse(
  readFileSync(fileURLToPath(new URL("manifest.json", fixturesDir)), "utf8")
) as Manifest;

let parsed: ParsedTrace[] = [];

beforeAll(async () => {
  parsed = await Promise.all(
    manifest.segments.map((s) => parseTraceBuffer(readFixture(s.file)))
  );
}, 60_000);

// ── The generated window set ─────────────────────────────────────────────

describe("generated window segments (real parse)", () => {
  it("every segment's events sit inside its declared monotonic window", () => {
    expect(parsed.length).toBe(manifest.segments.length);
    for (const [i, trace] of parsed.entries()) {
      const m = manifest.segments[i]!;
      expect(trace.events.length).toBeGreaterThan(0);
      expect(trace.minTs).toBeGreaterThanOrEqual(m.monoStartNs);
      expect(trace.maxTs).toBeLessThanOrEqual(m.monoEndNs);
    }
  });

  it("each segment's clock anchor maps its monotonic base to its key epoch", () => {
    for (const [i, trace] of parsed.entries()) {
      const m = manifest.segments[i]!;
      expect(trace.clockSyncAnchors.length).toBeGreaterThan(0);
      const a = trace.clockSyncAnchors[0]!;
      // realtime - monotonic exceeds 2^53, so compare in BigInt space.
      const offset = BigInt(Math.round(a.realtimeNs)) - BigInt(a.monotonicNs);
      expect(offset).toBe(BigInt(manifest.clockOffsetNs));
      expect(a.monotonicNs).toBe(m.monoStartNs);
      expect(m.epochS).toBe(manifest.epochBaseS + i * manifest.segmentSeconds);
    }
  });

  it("worker sets match the manifest, including the planted silence", () => {
    for (const [i, trace] of parsed.entries()) {
      const silent = manifest.plantedPolls
        .filter((p) => i > p.startSegment && i < p.endSegment)
        .map((p) => p.workerId);
      const expected = manifest.workers.filter((w) => !silent.includes(w));
      expect(segmentInvariants(trace).workerIds).toEqual(expected);
    }
  });

  it("edge extraction re-derives every planted boundary fact from the wire", () => {
    for (const p of manifest.plantedPolls) {
      const start = computeSegmentEdgePolls(parsed[p.startSegment]!);
      expect(start.openAtEnd).toContainEqual({
        workerId: p.workerId,
        start: p.startNs,
        taskId: p.taskId,
        spawnLocId: p.spawnLoc,
        spawnLoc: p.spawnLoc,
      });
      const end = computeSegmentEdgePolls(parsed[p.endSegment]!);
      expect(end.closeAtStart).toContainEqual({
        workerId: p.workerId,
        end: p.endNs,
      });
    }
  });

  it("stitches BOTH planted polls (adjacent and silent-interior) and nothing else", () => {
    const keys = manifest.segments.map((s) => s.key);
    const entries = new Map<string, SegmentEntry>(
      manifest.segments.map((s, i) => {
        const trace = parsed[i]!;
        return [
          s.key,
          {
            state: "parsed",
            extent: { startNs: s.monoStartNs, endNs: s.monoEndNs },
            sizeBytes: 1,
            trace,
            rawByteLength: 1,
            invariants: segmentInvariants(trace),
            edgePolls: computeSegmentEdgePolls(trace),
          } satisfies SegmentEntry,
        ];
      })
    );
    const { stitched, truncated } = computeWindowBoundaryPolls(keys, entries);
    const expected = manifest.plantedPolls.map((p) => ({
      workerId: p.workerId,
      start: p.startNs,
      end: p.endNs,
      taskId: p.taskId,
      spawnLocId: p.spawnLoc,
      spawnLoc: p.spawnLoc,
    }));
    expect(stitched).toEqual(expect.arrayContaining(expected));
    expect(stitched.length).toBe(expected.length);
    // Background activity never leaves stray evidence at the edges.
    expect(truncated).toEqual([]);
  });

  it("a non-resident interior segment downgrades the chain to an honest truncation", () => {
    // Drop seg4 (the silent interior of the seg3 -> seg5 poll) to "listed":
    // the chain loses its silence proof and must surface truncated, not
    // stitched - never fabricate a completed poll.
    const p = manifest.plantedPolls.find((x) => x.endSegment - x.startSegment > 1)!;
    const interior = manifest.segments[p.startSegment + 1]!;
    const keys = manifest.segments.map((s) => s.key);
    const entries = new Map<string, SegmentEntry>(
      manifest.segments.map((s, i) => {
        if (s.key === interior.key) {
          return [
            s.key,
            {
              state: "listed",
              extent: { startNs: s.monoStartNs, endNs: s.monoEndNs },
              sizeBytes: 1,
            } satisfies SegmentEntry,
          ];
        }
        const trace = parsed[i]!;
        return [
          s.key,
          {
            state: "parsed",
            extent: { startNs: s.monoStartNs, endNs: s.monoEndNs },
            sizeBytes: 1,
            trace,
            rawByteLength: 1,
            invariants: segmentInvariants(trace),
            edgePolls: computeSegmentEdgePolls(trace),
          } satisfies SegmentEntry,
        ];
      })
    );
    const { stitched, truncated } = computeWindowBoundaryPolls(keys, entries);
    expect(stitched.map((s) => s.taskId)).not.toContain(p.taskId);
    expect(truncated).toContainEqual(
      expect.objectContaining({
        workerId: p.workerId,
        taskId: p.taskId,
        truncatedAt: "end",
        openEnded: true,
      })
    );
  });
});

// ── Multi-runtime fixture ────────────────────────────────────────────────

describe("multi-runtime fixture (#596)", () => {
  interface RuntimeGroup {
    name: string;
    inferred: boolean;
    workerIds: number[];
  }
  const { computeRuntimeGroups } = require("../../../trace_analysis.js") as {
    computeRuntimeGroups: (
      workerIds: number[],
      runtimeWorkers?: Map<string, number[]>
    ) => RuntimeGroup[];
  };

  it("the runtime.<name> metadata parses into runtimeWorkers and groups lanes", async () => {
    const trace = await parseTraceBuffer(readFixture(manifest.multiRuntime.file));
    for (const [name, ids] of Object.entries(manifest.multiRuntime.runtimes)) {
      expect(trace.runtimeWorkers.get(name)).toEqual(ids);
    }

    const workerIds = [...new Set(trace.events.map((e) => e.workerId))].sort(
      (a, b) => a - b
    );
    const groups = computeRuntimeGroups(workerIds, trace.runtimeWorkers);
    expect(groups.map((g) => g.name)).toEqual([
      "main",
      ...Object.keys(manifest.multiRuntime.runtimes),
    ]);
    expect(groups[0]!.inferred).toBe(true);
    expect(groups[0]!.workerIds).toEqual(manifest.multiRuntime.mainWorkers);
    expect(groups[1]!.inferred).toBe(false);
    expect(groups[1]!.workerIds).toEqual(
      Object.values(manifest.multiRuntime.runtimes)[0]
    );
  });
});
