import { describe, expect, it } from "vitest";
import { buildExemplarViewerUrl } from "./exemplar-link.js";

const ROLE = "arn:aws:iam::111122223333:role/TraceReader";
const ROLE_SOURCE = {
  bucket: "scope-bucket",
  region: "us-west-2",
  credentials: { kind: "role" as const, roleArn: ROLE },
};
const AMBIENT_SOURCE = {
  bucket: "scope-bucket",
  region: "",
  credentials: { kind: "ambient" as const },
};

function viewerParams(link: string): URLSearchParams {
  return new URLSearchParams(link.slice("viewer.html?".length));
}

function traceParams(link: string): URLSearchParams {
  const trace = viewerParams(link).get("trace") ?? "";
  return new URLSearchParams(trace.slice("/api/object?".length));
}

describe("buildExemplarViewerUrl", () => {
  it("normalizes a fully-qualified span key and carries role identity", () => {
    const link = buildExemplarViewerUrl({
      exemplar: {
        source_key: "s3://actual-bucket/traces/segment.bin.gz",
        host: "host-a",
        start_ns: 1_000,
        end_ns: 2_000,
        span_name: "request",
      },
      source: ROLE_SOURCE,
      service: "demo-service",
    });
    const p = viewerParams(link);

    expect(traceParams(link).get("bucket")).toBe("actual-bucket");
    expect(traceParams(link).get("key")).toBe("traces/segment.bin.gz");
    expect(p.get("svc")).toBe("demo-service");
    expect(p.get("host")).toBe("host-a");
    expect(p.get("focus_start")).toBe("1000");
    expect(p.get("focus_end")).toBe("2000");
    expect(p.get("focus_span_name")).toBe("request");
    expect(p.get("aws_region")).toBe("us-west-2");
    expect(p.get("credential_mode")).toBe("role");
    expect(p.get("aws_role_arn")).toBe(ROLE);
  });

  it("uses an echoed bucket and carries poll worker/task focus", () => {
    const link = buildExemplarViewerUrl({
      exemplar: {
        source_key: "traces/segment.bin.gz",
        start_ns: 10,
        end_ns: 20,
        worker_id: 3,
        task_id: 42,
      },
      source: AMBIENT_SOURCE,
      objectBucket: "response-bucket",
    });
    const p = viewerParams(link);

    expect(traceParams(link).get("bucket")).toBe("response-bucket");
    expect(p.get("focus_worker")).toBe("3");
    expect(p.get("focus_task")).toBe("42");
    expect(p.get("credential_mode")).toBe("ambient");
  });

  it("reuses a raw trace and returns empty without any trace source", () => {
    const raw = buildExemplarViewerUrl({
      exemplar: {
        start_ns: 10,
        end_ns: 20,
        span_name: "RecordMetric",
      },
      source: AMBIENT_SOURCE,
      trace: "demo-trace.bin",
    });
    expect(viewerParams(raw).get("trace")).toBe("demo-trace.bin");
    expect(viewerParams(raw).get("focus_span_name")).toBe("RecordMetric");

    expect(
      buildExemplarViewerUrl({
        exemplar: { start_ns: 10 },
        source: AMBIENT_SOURCE,
      }),
    ).toBe("");
  });

  it("never serializes literal credential values", () => {
    const link = buildExemplarViewerUrl({
      exemplar: { source_key: "traces/segment.bin.gz" },
      source: {
        bucket: "literal-traces",
        region: "us-east-1",
        credentials: {
          kind: "literal",
          accessKeyId: "SYNTHETIC_ACCESS_KEY",
          secretAccessKey: "synthetic-secret-key",
          sessionToken: "synthetic-session-token",
        },
      },
    });

    expect(viewerParams(link).get("credential_mode")).toBe("literal");
    expect(viewerParams(link).get("aws_role_arn")).toBeNull();
    expect(link).not.toContain("SYNTHETIC_ACCESS_KEY");
    expect(link).not.toContain("synthetic-secret-key");
    expect(link).not.toContain("synthetic-session-token");
  });
});
