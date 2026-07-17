import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { gunzipSync } from "node:zlib";
import { beforeAll, describe, expect, it } from "vitest";
import { parseTraceBuffer } from "./load.js";
import { ColumnarCpuSamples, CpuSample } from "./columnar-cpu-samples.js";

let raw: Uint8Array;
beforeAll(() => {
  const b = readFileSync(fileURLToPath(new URL("../../../public/demo-trace.bin", import.meta.url)));
  raw = b[0] === 0x1f && b[1] === 0x8b ? new Uint8Array(gunzipSync(b)) : new Uint8Array(b);
});

describe("ColumnarCpuSamples sink parity", () => {
  it("produces identical cpu samples (fields + callchains) to the fat path", async () => {
    const fat = await parseTraceBuffer(raw);
    const col = await parseTraceBuffer(raw, { cpuSampleSink: new ColumnarCpuSamples() } as never);

    expect(col.cpuSamples.length).toBe(fat.cpuSamples.length);
    expect(fat.cpuSamples.length).toBeGreaterThan(0);
    expect(col.cpuSamples[0]).toBeInstanceOf(CpuSample);

    for (let i = 0; i < fat.cpuSamples.length; i++) {
      const a = fat.cpuSamples[i]!;
      const b = col.cpuSamples[i]!;
      expect(b.timestamp).toBe(a.timestamp);
      expect(b.workerId).toBe(a.workerId); // finalize resolves tid->worker on both
      expect(b.tid).toBe(a.tid);
      expect(b.source).toBe(a.source);
      expect(b.cpu).toBe(a.cpu);
      // callchain: the getter must reproduce the exact hex string array.
      expect(b.callchain).toStrictEqual(a.callchain);
    }
  });

  it("callchain getter yields a fresh equal array each access (value, not identity)", async () => {
    const col = await parseTraceBuffer(raw, { cpuSampleSink: new ColumnarCpuSamples() } as never);
    const s = col.cpuSamples.find((x) => x.callchain.length > 0);
    expect(s).toBeDefined();
    const c1 = s!.callchain;
    const c2 = s!.callchain;
    expect(c1).toStrictEqual(c2);
    expect(c1).not.toBe(c2); // fresh array (lazy), safe for value consumers
  });

  it("scalar fields stay mutable (attachCpuSamples writes spawnLoc/inPoll)", async () => {
    const col = await parseTraceBuffer(raw, { cpuSampleSink: new ColumnarCpuSamples() } as never);
    const s = col.cpuSamples[0]!;
    s.spawnLoc = "x::y";
    s.inPoll = true;
    s.workerId = 42;
    expect(s.spawnLoc).toBe("x::y");
    expect(s.inPoll).toBe(true);
    expect(s.workerId).toBe(42);
  });
});
