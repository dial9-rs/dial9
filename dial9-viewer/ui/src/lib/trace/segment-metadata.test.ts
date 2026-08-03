// Tests for the trace-embedded service/host identity + reconciliation. Pins
// the writer-side key-name contract, the embedded-wins-with-tooltip
// reconciliation rule, and walks both real sources: the committed demo trace
// (carries `service`, no `host`) and a window fixture (carries both).

import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { gunzipSync } from "node:zlib";
import { beforeAll, describe, expect, it } from "vitest";
import { parseTraceBuffer } from "./index.js";
import type { ParsedTrace } from "../../types/trace.js";
import {
  SEGMENT_HOST_KEY,
  SEGMENT_SERVICE_KEY,
  readKeyDerivedIdentity,
  readSegmentIdentity,
  readSegmentMetadataEntries,
  reconcileIdentity,
} from "./segment-metadata.js";

/** Parse a trace file (auto-gunzip on the 0x1f 0x8b magic) for a row-walk. */
async function parseFixture(relUrl: string): Promise<ParsedTrace> {
  const bytes = readFileSync(fileURLToPath(new URL(relUrl, import.meta.url)));
  const raw =
    bytes[0] === 0x1f && bytes[1] === 0x8b
      ? new Uint8Array(gunzipSync(bytes))
      : new Uint8Array(bytes);
  return parseTraceBuffer(raw);
}

const trace = (entries: Array<[string, string]>): ParsedTrace =>
  ({ segmentMetadata: new Map(entries) }) as unknown as ParsedTrace;

describe("segment-metadata key contract", () => {
  it("pins the writer-side key names literally", () => {
    // These match the dial9 writer/source convention (dial9-utils S3 source,
    // gen_fixtures standard_metadata()). Changing them is a read-contract break.
    expect(SEGMENT_SERVICE_KEY).toBe("service");
    expect(SEGMENT_HOST_KEY).toBe("host");
  });
});

describe("readSegmentIdentity", () => {
  it("returns {} for a null trace", () => {
    expect(readSegmentIdentity(null)).toEqual({});
  });
  it("reads service and host from the metadata map", () => {
    expect(
      readSegmentIdentity(
        trace([
          ["service", "checkout-api"],
          ["host", "i-0abc123"],
        ]),
      ),
    ).toEqual({ service: "checkout-api", host: "i-0abc123" });
  });
  it("omits a field with an empty value (present key, no identity)", () => {
    expect(readSegmentIdentity(trace([["service", ""]]))).toEqual({});
  });
  it("returns {} when the map declares neither key", () => {
    expect(
      readSegmentIdentity(trace([["process.available_parallelism", "8"]])),
    ).toEqual({});
  });
});

describe("readSegmentMetadataEntries", () => {
  it("returns every entry sorted by key without dropping empty values", () => {
    const value = trace([
      ["service", "checkout-api"],
      ["empty", ""],
      ["cpu.profile.backend", "perf"],
      ["host", "i-0abc123"],
    ]);

    expect(readSegmentMetadataEntries(value)).toEqual([
      ["cpu.profile.backend", "perf"],
      ["empty", ""],
      ["host", "i-0abc123"],
      ["service", "checkout-api"],
    ]);
    expect(readSegmentMetadataEntries(null)).toEqual([]);
  });
});

describe("readKeyDerivedIdentity", () => {
  it("parses svc/host from the viewer URL search string", () => {
    expect(readKeyDerivedIdentity("?svc=checkout-api&host=i-0abc123")).toEqual({
      service: "checkout-api",
      host: "i-0abc123",
    });
  });
  it("returns {} when the params are absent or empty", () => {
    expect(readKeyDerivedIdentity("")).toEqual({});
    expect(readKeyDerivedIdentity("?trace=foo.bin&svc=")).toEqual({});
  });
});

describe("reconcileIdentity (embedded wins, tooltip on disagreement)", () => {
  it("prefers embedded metadata and tooltips a disagreeing key-derived value", () => {
    const r = reconcileIdentity(
      { service: "real-svc", host: "real-host" },
      { service: "key-svc", host: "real-host" },
    );
    expect(r.service).toEqual({
      value: "real-svc",
      fromMetadata: true,
      keyDerived: "key-svc",
    });
    // Agreeing host carries no tooltip.
    expect(r.host).toEqual({ value: "real-host", fromMetadata: true });
    expect(r.host?.keyDerived).toBeUndefined();
  });
  it("uses embedded alone when there is no key-derived value", () => {
    const r = reconcileIdentity({ service: "svc" }, {});
    expect(r.service).toEqual({ value: "svc", fromMetadata: true });
  });
  it("falls back to the key-derived value when embedded is absent", () => {
    const r = reconcileIdentity({}, { service: "key-svc", host: "key-host" });
    expect(r.service).toEqual({ value: "key-svc", fromMetadata: false });
    expect(r.host).toEqual({ value: "key-host", fromMetadata: false });
  });
  it("yields {} when neither source has anything", () => {
    expect(reconcileIdentity({}, {})).toEqual({});
  });
});

describe("row-walk against real traces", () => {
  let demo: ParsedTrace;
  let windowSeg: ParsedTrace;
  beforeAll(async () => {
    demo = await parseFixture("../../../public/demo-trace.bin");
    windowSeg = await parseFixture(
      "../../../parity/fixtures/segments/window-00.bin.gz",
    );
  });

  it("surfaces `service` from the demo trace (host absent by design)", () => {
    // The demo is produced by the metrics-service example, which emits
    // `service` but no `host`; the host surface is walked via the fixture.
    const id = readSegmentIdentity(demo);
    expect(id.service).toBeTruthy();
    expect(id.host).toBeUndefined();
  });

  it("surfaces both `service` and `host` from a window fixture", () => {
    const id = readSegmentIdentity(windowSeg);
    expect(id.service).toBe("svc-fix");
    expect(id.host).toBe("window");
  });
});
