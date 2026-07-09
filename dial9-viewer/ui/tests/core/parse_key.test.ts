// Verify parseKey() in index.html understands both the new boot_id layout
// and the legacy (pre-#225) layout.
//
// Migrated from test_parse_key.js (T10). parseKey still lives inline in the
// legacy index.html, so this suite keeps the original extract-from-index.html
// mechanism (regex + vm sandbox) for now; T15 re-points it at lib/trace/keys.ts
// once the TS port of the key parser lands.

import { describe, it, expect, beforeAll } from "vitest";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import * as vm from "node:vm";

interface ParsedKey {
  service: string;
  host: string;
  bootId: string;
  epoch: number;
  segIndex: string;
}

type ParseKeyFn = (key: string) => ParsedKey;

let parseKey: ParseKeyFn;

beforeAll(() => {
  const html = readFileSync(
    fileURLToPath(new URL("../../index.html", import.meta.url)),
    "utf8",
  );
  const m = html.match(/function parseKey\([\s\S]*?\n    \}\n/);
  expect(m, "could not locate parseKey() in index.html").not.toBeNull();

  // parseKey depends on formatEpoch/formatDate - stub them out.
  const sandbox: { parseKey: ParseKeyFn | null; formatEpoch: () => string } = {
    parseKey: null,
    formatEpoch: () => "",
  };
  vm.createContext(sandbox);
  vm.runInContext(m![0] + "\nthis.parseKey = parseKey;", sandbox);
  expect(sandbox.parseKey).toBeTypeOf("function");
  parseKey = sandbox.parseKey!;
});

describe("parseKey (extracted from index.html)", () => {
  it("new layout with prefix", () => {
    const p = parseKey(
      "traces/2026-04-09/1910/checkout-api/us-east-1/abcd-123213/1744224000-3.bin.gz",
    );
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
    expect(p.service, "new no-prefix: service").toBe("checkout-api");
    expect(p.host, "new no-prefix: host").toBe("us-east-1");
    expect(p.bootId, "new no-prefix: bootId").toBe("xyzw-asdfasdf");
  });

  it("legacy layout with prefix - unchanged behavior", () => {
    const p = parseKey(
      "traces/2026-04-09/1910/checkout-api/host1/1744224000-2.bin.gz",
    );
    expect(p.service, "legacy: service").toBe("checkout-api");
    expect(p.host, "legacy: host").toBe("host1");
    expect(p.bootId, "legacy: bootId empty").toBe("");
    expect(p.epoch, "legacy: epoch").toBe(1744224000);
    expect(p.segIndex, "legacy: segIndex").toBe("2");
  });

  it("compound-instance: returns object (best-effort)", () => {
    // Instance path with embedded slash is a best-effort legacy case -
    // cannot be reliably distinguished from the new boot_id layout on
    // path-component count alone, so the parser falls back to the positional
    // heuristic. We just sanity-check that parsing does not throw.
    const p = parseKey(
      "traces/2026-04-09/1910/checkout-api/us-east-1/i-0abc123/1744224000-0.bin.gz",
    );
    expect(p, "compound-instance: must return object").toBeTypeOf("object");
    expect(p).not.toBeNull();
  });
});
