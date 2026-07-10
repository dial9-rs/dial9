// Tests for the T14 legacy-compat key layer (legacy-keys.ts). The whole
// point of this module is BYTE-IDENTICAL outputs to the legacy inline
// parseKey/traceTitleParams (T15 retires it), so the reference
// implementation below is the legacy index.html algorithm transcribed
// verbatim, and parseKeyCompat is property-compared against it over a
// corpus that covers every branch.

import { describe, expect, it } from "vitest";
import { extractPrefix, parseKeyCompat, titleParamsCompat } from "./legacy-keys.js";
import { formatEpochStr } from "./format.js";

/** The legacy index.html parseKey (index.html:1007-1059), transcribed. */
function legacyParseKey(key: string): {
  service: string;
  host: string;
  bootId: string;
  epoch: number;
  segIndex: string;
} {
  const parts = key.split("/");
  const dateRe = /^\d{4}-\d{2}-\d{2}$/;
  let dateIdx = -1;
  for (let i = parts.length - 1; i >= 0; i--) {
    if (dateRe.test(parts[i]!)) {
      dateIdx = i;
      break;
    }
  }
  const file = parts[parts.length - 1]!;
  const match = file.match(/^(\d+)-(\d+)\.bin/);
  let epoch = 0;
  let segIndex = "";
  if (match) {
    epoch = parseInt(match[1]!, 10);
    segIndex = match[2]!;
  }
  if (dateIdx >= 0) {
    const below = parts.length - 1 - dateIdx;
    if (below === 5) {
      return {
        service: parts[dateIdx + 2]!,
        host: parts[dateIdx + 3]!,
        bootId: parts[dateIdx + 4]!,
        epoch,
        segIndex,
      };
    }
    if (below === 4) {
      return {
        service: parts[dateIdx + 2]!,
        host: parts[dateIdx + 3]!,
        bootId: "",
        epoch,
        segIndex,
      };
    }
  }
  if (parts.length >= 5) {
    return {
      service: parts[parts.length - 3]!,
      host: parts[parts.length - 2]!,
      bootId: "",
      epoch,
      segIndex,
    };
  }
  return { service: "", host: key, bootId: "", epoch: 0, segIndex: "" };
}

// One key per reachable branch, plus the dev-seed shapes the walkers assert.
const CORPUS = [
  // #225 default layout (5 components below the date).
  "traces/2026-04-09/1900/checkout-api/i-0abc/boot-1/1744221600-0.bin.gz",
  // Legacy pre-#225 layout (4 below, no boot id).
  "traces/2026-04-09/1900/checkout-api/i-0abc/1744221600-3.bin",
  // Date at root (no prefix), #225 layout.
  "2026-04-09/1900/svc/host/boot/1744221600-1.bin.gz",
  // The dev-seed demo key: SIX components below the date -> the legacy
  // positional fallback mislabels it (Finding 1).
  "traces/2026-04-09/1900/demo-service/local/host-0/abcd/1744221600-0.bin.gz",
  // Date present but too few components for any layout.
  "2026-04-09/1744221600-0.bin.gz",
  // No date-shaped segment, >= 5 components: positional best-effort.
  "custom/deep/prefix/svc-a/host-b/1744221600-7.bin.gz",
  // No date, < 5 components: whole key in the host column.
  "some/file.bin",
  "plain-key",
  // Filename without the epoch-index pattern.
  "traces/2026-04-09/1900/svc/host/boot/checkpoint.bin.gz",
  // Trailing slash produces an empty filename component.
  "traces/2026-04-09/1900/svc/host/",
];

describe("parseKeyCompat is byte-identical to the legacy parseKey", () => {
  for (const key of CORPUS) {
    it(key, () => {
      expect(parseKeyCompat(key)).toEqual(legacyParseKey(key));
    });
  }

  it("mislabels the dev-seed demo key exactly as recorded (Finding 1)", () => {
    const p = parseKeyCompat(
      "traces/2026-04-09/1900/demo-service/local/host-0/abcd/1744221600-0.bin.gz",
    );
    expect(p.service).toBe("host-0");
    expect(p.host).toBe("abcd");
    expect(p.bootId).toBe("");
    expect(p.epoch).toBe(1744221600);
    expect(p.segIndex).toBe("0");
  });
});

describe("extractPrefix (features/01 I8)", () => {
  it("returns everything before the first date segment", () => {
    expect(extractPrefix("traces/2026-04-09/1900/svc/host/x.bin")).toBe("traces");
    expect(extractPrefix("a/b/2026-01-02/rest")).toBe("a/b");
  });
  it("returns the empty string at a date root or with no date", () => {
    expect(extractPrefix("2026-04-09/1900/svc/host/x.bin")).toBe("");
    expect(extractPrefix("no/date/here.bin")).toBe("");
  });
});

describe("titleParamsCompat mirrors the legacy traceTitleParams", () => {
  const demoKey =
    "traces/2026-04-09/1900/demo-service/local/host-0/abcd/1744221600-0.bin.gz";

  it("feeds the legacy (mislabeled) fields into the title", () => {
    const p = titleParamsCompat([demoKey], false);
    expect(p.get("svc")).toBe("host-0");
    expect(p.get("host")).toBe("abcd");
    expect(p.get("from")).toBe(formatEpochStr(1744221600, false));
    expect(p.get("to")).toBeNull();
    expect(p.get("segs")).toBe("1");
  });

  it("drops host on multi-host selections and spans from/to", () => {
    const keys = [
      "traces/2026-04-09/1900/svc/host-a/boot/1744221600-0.bin.gz",
      "traces/2026-04-09/1910/svc/host-b/boot/1744222200-0.bin.gz",
    ];
    const p = titleParamsCompat(keys, false);
    expect(p.get("svc")).toBe("svc");
    expect(p.get("host")).toBeNull();
    expect(p.get("from")).toBe(formatEpochStr(1744221600, false));
    expect(p.get("to")).toBe(formatEpochStr(1744222200, false));
    expect(p.get("segs")).toBe("2");
  });

  it("keys that parse to nothing still count toward segs", () => {
    const p = titleParamsCompat(["plain-key"], false);
    expect(p.get("svc")).toBeNull();
    // Legacy: host = the whole key for the last-resort branch.
    expect(p.get("host")).toBe("plain-key");
    expect(p.get("from")).toBeNull();
    expect(p.get("segs")).toBe("1");
  });
});
