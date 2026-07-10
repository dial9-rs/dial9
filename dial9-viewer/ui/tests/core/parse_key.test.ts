// Verify the S3 trace-key parser understands both the new boot_id layout
// and the legacy (pre-#225) layout.
//
// Migrated from test_parse_key.js (T10). Originally extracted parseKey()
// from the legacy index.html inline script via regex + vm sandbox; T15
// re-pointed it at the TS port, lib/trace/keys.ts, which the migrated
// browser page consumes (closing T10's interim note). The legacy inline
// copy still exists in index.html but is no longer under test here - the
// typed parser is the single implementation going forward, and the legacy
// page's recorded behavior is asserted by the parity row-walker instead.
//
// The typed parser returns a { layout: "known" | "unknown" } discriminated
// union (the ADR-0004 section 1 defect fix); the original T10 cases below
// all target documented layouts, which parse as layout: "known" with the
// same fields the inline parser produced.

import { describe, it, expect } from "vitest";
import { parseKey } from "../../src/lib/trace/keys.js";

describe("parseKey (lib/trace/keys.ts)", () => {
  it("new layout with prefix", () => {
    const p = parseKey(
      "traces/2026-04-09/1910/checkout-api/us-east-1/abcd-123213/1744224000-3.bin.gz",
    );
    expect(p.layout, "new layout: known").toBe("known");
    if (p.layout !== "known") return;
    expect(p.service, "new layout: service").toBe("checkout-api");
    expect(p.host, "new layout: host").toBe("us-east-1");
    expect(p.bootId, "new layout: bootId").toBe("abcd-123213");
    expect(p.epoch, "new layout: epoch").toBe(1744224000);
    expect(p.segIndex, "new layout: segIndex").toBe("3");
  });

  it("new layout without prefix", () => {
    const p = parseKey(
      "2026-04-09/1910/checkout-api/us-east-1/xyzw-asdfasdf/1744224000-0.bin.gz",
    );
    expect(p.layout, "new no-prefix: known").toBe("known");
    if (p.layout !== "known") return;
    expect(p.service, "new no-prefix: service").toBe("checkout-api");
    expect(p.host, "new no-prefix: host").toBe("us-east-1");
    expect(p.bootId, "new no-prefix: bootId").toBe("xyzw-asdfasdf");
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
    // cannot be reliably distinguished from the new boot_id layout on
    // path-component count alone, so it parses as the #225 layout. We just
    // sanity-check that parsing does not throw and yields a parsed object.
    const p = parseKey(
      "traces/2026-04-09/1910/checkout-api/us-east-1/i-0abc123/1744224000-0.bin.gz",
    );
    expect(p, "compound-instance: must return object").toBeTypeOf("object");
    expect(p).not.toBeNull();
    expect(p.layout, "compound-instance: parses as a documented layout").toBe("known");
  });
});
