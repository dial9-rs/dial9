import { describe, it, expect } from "vitest";
import { parseKey } from "../../src/lib/trace/keys.js";

describe("parseKey (lib/trace/keys.ts)", () => {
  it("historical boot-id layout with prefix", () => {
    const p = parseKey(
      "traces/2026-04-09/1910/checkout-api/us-east-1/abcd-123213/1744224000-3.bin.gz",
    );
    expect(p.layout, "historical layout: known").toBe("known");
    if (p.layout !== "known") return;
    expect(p.service, "historical layout: service").toBe("checkout-api");
    expect(p.host, "historical layout: host").toBe("us-east-1");
    expect(p.bootId, "historical layout: bootId").toBe("abcd-123213");
    expect(p.epoch, "historical layout: epoch").toBe(1744224000);
    expect(p.segIndex, "historical layout: segIndex").toBe("3");
  });

  it("historical boot-id layout without prefix", () => {
    const p = parseKey(
      "2026-04-09/1910/checkout-api/us-east-1/xyzw-asdfasdf/1744224000-0.bin.gz",
    );
    expect(p.layout, "historical no-prefix: known").toBe("known");
    if (p.layout !== "known") return;
    expect(p.service, "historical no-prefix: service").toBe("checkout-api");
    expect(p.host, "historical no-prefix: host").toBe("us-east-1");
    expect(p.bootId, "historical no-prefix: bootId").toBe("xyzw-asdfasdf");
  });

  it("legacy layout with prefix - unchanged behavior", () => {
    const p = parseKey(
      "traces/2026-04-09/1910/checkout-api/host1/1744224000-2.bin.gz",
    );
    expect(p.layout, "legacy: known").toBe("known");
    if (p.layout !== "known") return;
    expect(p.service, "legacy: service").toBe("checkout-api");
    expect(p.host, "legacy: host").toBe("host1");
    expect(p.bootId, "legacy: bootId empty").toBe("");
    expect(p.epoch, "legacy: epoch").toBe(1744224000);
    expect(p.segIndex, "legacy: segIndex").toBe("2");
  });

  it("compound-instance: returns object (best-effort)", () => {
    // Instance path with embedded slash is a best-effort legacy case -
    // cannot be reliably distinguished from the historical boot_id layout on
    // path-component count alone. We just
    // sanity-check that parsing does not throw and yields a parsed object.
    const p = parseKey(
      "traces/2026-04-09/1910/checkout-api/us-east-1/i-0abc123/1744224000-0.bin.gz",
    );
    expect(p, "compound-instance: must return object").toBeTypeOf("object");
    expect(p).not.toBeNull();
    expect(p.layout, "compound-instance: parses as a documented layout").toBe("known");
  });
});
