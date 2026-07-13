// title.ts tests (T09 DoD): features/01 I3 single- and multi-host cases,
// plus epoch window and unknown-key handling.

import { describe, expect, it } from "vitest";
import { traceTitleParams } from "./title.js";

const KEY_A_HOST1 =
  "traces/2026-04-09/1900/checkout-api/host1/boot-a/1744224000-0.bin.gz";
const KEY_B_HOST1 =
  "traces/2026-04-09/1910/checkout-api/host1/boot-a/1744224600-1.bin.gz";
const KEY_C_HOST2 =
  "traces/2026-04-09/1910/checkout-api/host2/boot-b/1744224600-0.bin.gz";
const KEY_OTHER_SVC =
  "traces/2026-04-09/1910/billing/host1/boot-c/1744224600-2.bin.gz";

describe("traceTitleParams (features/01 I3)", () => {
  it("single-host: svc, host, from/to window, segs", () => {
    const p = traceTitleParams([KEY_A_HOST1, KEY_B_HOST1]);
    expect(p.get("svc")).toBe("checkout-api");
    expect(p.get("host")).toBe("host1");
    expect(p.get("from")).toBe("2025-04-09 18:40:00");
    expect(p.get("to")).toBe("2025-04-09 18:50:00");
    expect(p.get("segs")).toBe("2");
  });

  it("multi-host: host param is dropped", () => {
    const p = traceTitleParams([KEY_A_HOST1, KEY_C_HOST2]);
    expect(p.get("svc")).toBe("checkout-api");
    expect(p.get("host")).toBeNull();
    expect(p.get("segs")).toBe("2");
  });

  it("multiple services join with ', '", () => {
    const p = traceTitleParams([KEY_A_HOST1, KEY_OTHER_SVC]);
    expect(p.get("svc")).toBe("checkout-api, billing");
  });

  it("single epoch: from only, no to", () => {
    const p = traceTitleParams([KEY_A_HOST1]);
    expect(p.get("from")).toBe("2025-04-09 18:40:00");
    expect(p.get("to")).toBeNull();
  });

  it("duplicate epochs across hosts still produce a from/to window", () => {
    // Legacy sorts ALL epochs (not deduped): two segments at the same
    // minute plus one earlier yields from=min, to=max.
    const p = traceTitleParams([KEY_A_HOST1, KEY_B_HOST1, KEY_C_HOST2]);
    expect(p.get("from")).toBe("2025-04-09 18:40:00");
    expect(p.get("to")).toBe("2025-04-09 18:50:00");
    expect(p.get("segs")).toBe("3");
  });

  it("unknown-layout keys contribute no svc/host, but keep the epoch window (T15)", () => {
    const demoKey =
      "traces/2026-04-09/1900/demo-service/local/host-0/abcd/1744224000-0.bin.gz";
    const p = traceTitleParams([demoKey]);
    expect(p.get("svc")).toBeNull();
    expect(p.get("host")).toBeNull();
    // The filename epoch is layout-independent: the from window survives.
    expect(p.get("from")).toBe("2025-04-09 18:40:00");
    expect(p.get("segs")).toBe("1");
  });

  it("empty selection: segs=0 and nothing else", () => {
    const p = traceTitleParams([]);
    expect([...p.keys()]).toEqual(["segs"]);
    expect(p.get("segs")).toBe("0");
  });
});
