import { describe, expect, it } from "vitest";
import {
  AMBIENT_CREDENTIALS,
  EMPTY_LITERAL_CREDENTIALS,
  applyToCreds,
  credentialHeadersForSource,
  makeSourceScope,
  readNamespacedSourceScope,
  readPlainSourceScope,
  toShareableSourceScope,
  toUrlSourceScope,
  writeNamespacedParams,
  writeNamespacedUrlParams,
  writeRequestParams,
  writeShareableParams,
  writeUrlParams,
  type ShareableSourceScope,
  type SourceScope,
  type SourceScopeCredentials,
  type StoredSourceCredentials,
} from "./source-scope.js";

const ROLE = "arn:aws:iam::123456789012:role/Dial9TraceReader";
const SECRET = "SECRET-CANARY-DO-NOT-SERIALIZE";
const LITERAL: SourceScope = {
  bucket: "cell1-prod-pdx-dial9-traces",
  region: "us-west-2",
  credentials: {
    kind: "literal",
    accessKeyId: "AKIA-CANARY",
    secretAccessKey: SECRET,
    sessionToken: "TOKEN-CANARY",
  },
};
const ROLE_SCOPE: SourceScope = {
  bucket: LITERAL.bucket,
  region: LITERAL.region,
  credentials: { kind: "role", roleArn: ROLE },
};

function assertNoSecrets(params: URLSearchParams): void {
  const text = params.toString();
  expect(text).not.toContain("AKIA-CANARY");
  expect(text).not.toContain(SECRET);
  expect(text).not.toContain("TOKEN-CANARY");
}

describe("canonical SourceScope", () => {
  it("normalizes source fields and defaults explicitly to ambient", () => {
    expect(makeSourceScope(null, undefined)).toEqual({
      bucket: "",
      region: "",
      credentials: { kind: "ambient" },
    });
  });

  it("projects literal state to a URL marker without secret values", () => {
    expect(toUrlSourceScope(LITERAL)).toEqual({
      bucket: LITERAL.bucket,
      region: LITERAL.region,
      credentials: { kind: "literal" },
    });
  });

  it("makes literal state non-shareable while ambient and role remain shareable", () => {
    expect(toShareableSourceScope(LITERAL)).toEqual({ kind: "literal-credentials" });
    expect(toShareableSourceScope(ROLE_SCOPE)).toEqual({
      kind: "shareable",
      scope: ROLE_SCOPE,
    });
  });
});

describe("explicit URL credential modes", () => {
  it("plain URLs round-trip ambient explicitly", () => {
    const p = new URLSearchParams();
    const ambient = makeSourceScope("bucket", "us-east-1", AMBIENT_CREDENTIALS);
    writeUrlParams(p, ambient);
    expect(p.get("credential_mode")).toBe("ambient");
    expect(readPlainSourceScope(p, LITERAL)).toEqual(ambient);
  });

  it("plain URLs round-trip role and retain its safe ARN", () => {
    const p = new URLSearchParams();
    writeUrlParams(p, ROLE_SCOPE);
    expect(p.get("credential_mode")).toBe("role");
    expect(p.get("aws_role_arn")).toBe(ROLE);
    expect(readPlainSourceScope(p)).toEqual(ROLE_SCOPE);
  });

  it("plain literal URLs retain stored keys without serializing them", () => {
    const p = new URLSearchParams();
    writeUrlParams(p, LITERAL);
    expect(p.get("credential_mode")).toBe("literal");
    assertNoSecrets(p);
    expect(readPlainSourceScope(p, LITERAL)).toEqual(LITERAL);
  });

  it("literal mode without stored keys remains explicit but unconfigured", () => {
    const p = new URLSearchParams("bucket=b&credential_mode=literal");
    expect(readPlainSourceScope(p)).toEqual({
      bucket: "b",
      region: "",
      credentials: EMPTY_LITERAL_CREDENTIALS,
    });
  });

  it("legacy role ARN implies role; legacy no-role keeps stored literal", () => {
    expect(readPlainSourceScope(new URLSearchParams(`bucket=b&aws_role_arn=${ROLE}`))).toEqual({
      bucket: "b",
      region: "",
      credentials: { kind: "role", roleArn: ROLE },
    });
    expect(readPlainSourceScope(new URLSearchParams("bucket=b"), LITERAL).credentials).toEqual(
      LITERAL.credentials,
    );
  });

  it("contradictory explicit ambient ignores a stale role ARN", () => {
    const p = new URLSearchParams(`credential_mode=ambient&aws_role_arn=${ROLE}`);
    expect(readPlainSourceScope(p).credentials).toEqual(AMBIENT_CREDENTIALS);
  });

  it("namespaced URLs use s_credential_mode and never leak literals", () => {
    const p = new URLSearchParams();
    writeNamespacedUrlParams(p, LITERAL);
    expect(p.get("s_credential_mode")).toBe("literal");
    assertNoSecrets(p);
    expect(readNamespacedSourceScope(p, LITERAL)).toEqual(LITERAL);
  });
});

describe("share-only and request writers", () => {
  it("share-only writers accept ambient/role projections", () => {
    const shareable = toShareableSourceScope(ROLE_SCOPE);
    expect(shareable.kind).toBe("shareable");
    if (shareable.kind !== "shareable") return;
    const plain = new URLSearchParams();
    const namespaced = new URLSearchParams();
    writeShareableParams(plain, shareable.scope);
    writeNamespacedParams(namespaced, shareable.scope);
    expect(plain.get("credential_mode")).toBe("role");
    expect(namespaced.get("s_credential_mode")).toBe("role");
  });

  it("request URLs carry bucket+region but never role or credential mode", () => {
    const p = new URLSearchParams();
    writeRequestParams(p, ROLE_SCOPE);
    expect(p.get("bucket")).toBe(ROLE_SCOPE.bucket);
    expect(p.get("aws_region")).toBe(ROLE_SCOPE.region);
    expect(p.has("aws_role_arn")).toBe(false);
    expect(p.has("credential_mode")).toBe(false);
  });

  it("type-level share contract excludes literal credentials", () => {
    const valid: ShareableSourceScope = {
      bucket: ROLE_SCOPE.bucket,
      region: ROLE_SCOPE.region,
      credentials: { kind: "role", roleArn: ROLE },
    };
    expect(valid.credentials.kind).toBe("role");
    // @ts-expect-error literal credentials are intentionally not shareable
    const invalid: ShareableSourceScope = LITERAL;
    expect(invalid.credentials.kind).toBe("literal");
  });
});

describe("request headers", () => {
  it("ambient emits none; literal and role emit exactly one transport", () => {
    expect(credentialHeadersForSource(makeSourceScope("b", "r"))).toEqual({});
    expect(credentialHeadersForSource(LITERAL)).toEqual({
      "x-dial9-aws-access-key-id": "AKIA-CANARY",
      "x-dial9-aws-secret-access-key": SECRET,
      "x-dial9-aws-session-token": "TOKEN-CANARY",
      "x-dial9-aws-region": "us-west-2",
    });
    expect(credentialHeadersForSource(ROLE_SCOPE)).toEqual({
      "x-dial9-aws-role-arn": ROLE,
      "x-dial9-aws-region": "us-west-2",
    });
  });
});

describe("applyToCreds", () => {
  function fakeCreds(initial: StoredSourceCredentials): {
    creds: SourceScopeCredentials;
    calls: string[];
  } {
    let stored = initial;
    const calls: string[] = [];
    return {
      calls,
      creds: {
        get: () => stored,
        setAmbient() {
          calls.push("ambient");
          stored = { kind: "ambient" };
          return stored;
        },
        setLiteralMode() {
          calls.push("literal");
          stored = { ...EMPTY_LITERAL_CREDENTIALS };
          return stored;
        },
        setRoleArn(roleArn, opts) {
          calls.push(`role:${roleArn}:${opts?.region ?? ""}`);
          stored = opts?.region
            ? { kind: "role", roleArn, region: opts.region }
            : { kind: "role", roleArn };
          return stored;
        },
        setRegion(region) {
          calls.push(`region:${region}`);
          stored = { ...stored, region };
          return stored;
        },
      },
    };
  }

  it("activates ambient, literal, and role explicitly", () => {
    const ambient = fakeCreds({ ...LITERAL.credentials, region: LITERAL.region });
    applyToCreds(makeSourceScope("b", "r"), ambient.creds);
    expect(ambient.calls).toEqual(["ambient"]);

    const literal = fakeCreds({ kind: "ambient" });
    applyToCreds(makeSourceScope("b", "r", EMPTY_LITERAL_CREDENTIALS), literal.creds);
    expect(literal.calls).toEqual(["literal", "region:r"]);

    const role = fakeCreds({ kind: "ambient" });
    applyToCreds(ROLE_SCOPE, role.creds);
    expect(role.calls).toEqual([`role:${ROLE}:us-west-2`]);
  });
});
