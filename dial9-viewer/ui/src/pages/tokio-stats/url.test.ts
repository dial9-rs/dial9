import { describe, expect, it } from "vitest";
import {
  buildSyncQuery,
  parseInitialPeriods,
  readScope,
  scopeFromParams,
  shouldAutoLoad,
  type ScopeParams,
} from "./url.js";

const ambientSource = (bucket = "") => ({
  bucket,
  region: "",
  credentials: { kind: "ambient" as const },
});

const scope = (over: Partial<ScopeParams> = {}): ScopeParams => ({
  source: ambientSource("b"),
  prefix: "pre",
  service: "svc",
  host: ["h1", "h2"],
  ...over,
});

describe("readScope", () => {
  it("reads canonical source plus repeated hosts", () => {
    const p = new URLSearchParams(
      "?bucket=b&aws_region=us-west-2&credential_mode=ambient&prefix=pre&service=svc&host=h1&host=h2",
    );
    expect(readScope(p)).toEqual({
      source: {
        bucket: "b",
        region: "us-west-2",
        credentials: { kind: "ambient" },
      },
      prefix: "pre",
      service: "svc",
      host: ["h1", "h2"],
    });
  });

  it("legacy role ARN implies role mode", () => {
    const p = new URLSearchParams(
      "?bucket=b&aws_role_arn=arn:aws:iam::123456789012:role/Dial9TraceReader",
    );
    expect(readScope(p).source.credentials).toEqual({
      kind: "role",
      roleArn: "arn:aws:iam::123456789012:role/Dial9TraceReader",
    });
  });

  it("absent params are explicit ambient", () => {
    expect(readScope(new URLSearchParams(""))).toEqual({
      source: ambientSource(),
      prefix: null,
      service: null,
      host: [],
    });
  });
});

describe("scopeFromParams", () => {
  it("pulls an independent source from a diff side", () => {
    const side = new URLSearchParams("bucket=B&prefix=p2&service=svc-b&host=hb&start_ns=9");
    expect(scopeFromParams(side)).toEqual({
      source: ambientSource("B"),
      prefix: "p2",
      service: "svc-b",
      host: ["hb"],
    });
  });
});

describe("parseInitialPeriods", () => {
  it("restores multiple periods", () => {
    const p = new URLSearchParams("?p1_start_ns=100&p1_end_ns=200&p2_start_ns=300");
    expect(parseInitialPeriods(p)).toEqual([
      { startNs: "100", endNs: "200" },
      { startNs: "300", endNs: null },
    ]);
  });

  it("falls back to one legacy period and defaults to one blank period", () => {
    expect(parseInitialPeriods(new URLSearchParams("?start_ns=5&end_ns=6"))).toEqual([
      { startNs: "5", endNs: "6" },
    ]);
    expect(parseInitialPeriods(new URLSearchParams(""))).toEqual([
      { startNs: null, endNs: null },
    ]);
  });

  it("caps restore at ten periods", () => {
    const parts: string[] = [];
    for (let i = 1; i <= 11; i++) parts.push(`p${i}_start_ns=${i}`);
    expect(parseInitialPeriods(new URLSearchParams(parts.join("&")))).toHaveLength(10);
  });
});

describe("buildSyncQuery", () => {
  it("keeps the full source and period bounds with explicit ambient mode", () => {
    const periods = [
      { startNs: "100", endNs: "200" },
      { startNs: "300", endNs: null },
    ];
    expect(buildSyncQuery(scope(), periods)).toBe(
      "bucket=b&credential_mode=ambient&prefix=pre&service=svc&host=h1&host=h2" +
        "&p1_start_ns=100&p1_end_ns=200&p2_start_ns=300",
    );
  });

  it("writes explicit ambient even when source fields and bounds are empty", () => {
    expect(
      buildSyncQuery(
        scope({ source: ambientSource(), prefix: null, service: null, host: [] }),
        [{ startNs: null, endNs: null }],
      ),
    ).toBe("credential_mode=ambient");
  });

  it("carries region and role safely and round-trips", () => {
    const roleScope = scope({
      source: {
        bucket: "b",
        region: "us-west-2",
        credentials: {
          kind: "role",
          roleArn: "arn:aws:iam::123456789012:role/Dial9TraceReader",
        },
      },
      prefix: null,
      service: null,
      host: [],
    });
    const p = new URLSearchParams(buildSyncQuery(roleScope, [{ startNs: "5", endNs: null }]));
    expect(p.get("credential_mode")).toBe("role");
    expect(p.get("aws_region")).toBe("us-west-2");
    expect(p.get("aws_role_arn")).toBe(
      "arn:aws:iam::123456789012:role/Dial9TraceReader",
    );
    expect(readScope(p).source).toEqual(roleScope.source);
  });

  it("round-trips source and periods", () => {
    const original = scope();
    const periods = [
      { startNs: "100", endNs: "200" },
      { startNs: "300", endNs: null },
    ];
    const p = new URLSearchParams(buildSyncQuery(original, periods));
    expect(readScope(p)).toEqual(original);
    expect(parseInitialPeriods(p)).toEqual(periods);
  });
});

describe("shouldAutoLoad", () => {
  it("recognizes original bucket/start selectors", () => {
    expect(shouldAutoLoad(new URLSearchParams("?bucket=b"))).toBe(true);
    expect(shouldAutoLoad(new URLSearchParams("?start_ns=5"))).toBe(true);
    expect(shouldAutoLoad(new URLSearchParams("?start_ns="))).toBe(false);
  });
});
