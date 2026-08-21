import { describe, expect, it } from "vitest";
import {
  spanExemplarViewerUrl,
  type DetailModel,
} from "./detail.js";
import type { Exemplar } from "../../lib/trace/index.js";

const ROLE = "arn:aws:iam::111122223333:role/TraceReader";
const ROLE_SOURCE = {
  bucket: "demo-traces",
  region: "us-west-2",
  credentials: { kind: "role" as const, roleArn: ROLE },
};

function exemplar(overrides: Partial<Exemplar> = {}): Exemplar {
  return {
    elapsed_ns: 1_000,
    span_uid: "span-1",
    host: "host-a",
    start_ns: 1_000,
    end_ns: 2_000,
    source_key: "s3://demo-traces/traces/segment.bin.gz",
    attributes: [],
    ...overrides,
  };
}

describe("Span Explorer exemplar links", () => {
  it("carries the page reader role into viewer jumps", () => {
    const model = {
      linkState: {
        bucket: "demo-traces",
        region: "us-west-2",
        credentialMode: "role",
        roleArn: ROLE,
        service: "demo-service",
      },
      source: ROLE_SOURCE,
      rawTrace: null,
      spanType: { name: "request" },
    } as DetailModel;

    const link = spanExemplarViewerUrl(exemplar(), model);
    const p = new URL(link, "https://viewer.example").searchParams;

    expect(p.get("aws_region")).toBe("us-west-2");
    expect(p.get("credential_mode")).toBe("role");
    expect(p.get("aws_role_arn")).toBe(ROLE);
  });

  it("carries the reader role for raw-trace span jumps too", () => {
    const model = {
      linkState: null,
      source: ROLE_SOURCE,
      rawTrace: "/api/object?bucket=demo-traces&key=traces%2Fsegment.bin.gz",
      spanType: { name: "request" },
    } as DetailModel;

    const link = spanExemplarViewerUrl(exemplar(), model);
    const p = new URL(link, "https://viewer.example").searchParams;

    expect(p.get("trace")).toContain("/api/object");
    expect(p.get("credential_mode")).toBe("role");
    expect(p.get("aws_role_arn")).toBe(ROLE);
  });

  it("never projects literal credential values into a span jump", () => {
    const model = {
      linkState: null,
      source: {
        bucket: "demo-traces",
        region: "us-west-2",
        credentials: {
          kind: "literal",
          accessKeyId: "SYNTHETIC_ACCESS_KEY",
          secretAccessKey: "synthetic-secret-key",
          sessionToken: "synthetic-session-token",
        },
      },
      rawTrace: "/api/object?bucket=demo-traces&key=traces%2Fsegment.bin.gz",
      spanType: { name: "request" },
    } as DetailModel;

    const link = spanExemplarViewerUrl(exemplar(), model);

    expect(link).toContain("credential_mode=literal");
    expect(link).not.toContain("SYNTHETIC_ACCESS_KEY");
    expect(link).not.toContain("synthetic-secret-key");
    expect(link).not.toContain("synthetic-session-token");
  });
});
