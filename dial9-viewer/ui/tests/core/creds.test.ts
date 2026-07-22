// Tests for creds.js - the bring-your-own-credentials store and its stable
// scripting API (window.Dial9Creds.set/get/clear/headers). Runs in Node with an
// injected fake storage backend.
//
// Migrated from test_creds.js (T10): test_harness.js test/testAsync/assert
// rewritten to describe/it/expect; frozen core loaded via createRequire (see
// format.test.ts for the rationale). fetch stubbing uses vi.stubGlobal with
// the same install-in-try / restore-in-finally shape as the original.

import { describe, it, expect, vi } from "vitest";
import { createRequire } from "node:module";

const require = createRequire(import.meta.url);

interface StoredCreds {
  accessKeyId: string;
  secretAccessKey: string;
  sessionToken?: string;
  region?: string;
}

interface SetResult {
  ok: boolean;
  region?: string | null;
  error?: string | null;
}

interface BucketInfo {
  name: string;
  region: string | null;
}

interface StorageLike {
  getItem: (k: string) => string | null;
  setItem: (k: string, v: string) => void;
  removeItem: (k: string) => void;
}

const { Dial9Creds } = require("../../creds.js") as {
  Dial9Creds: {
    _setStorage: (s: StorageLike) => void;
    has: () => boolean;
    get: () => (StoredCreds & { kind?: string; roleArn?: string }) | null;
    set: (creds: Partial<StoredCreds>) => Promise<SetResult>;
    setRoleArn: (arn: string, opts?: { region?: string }) => void;
    setRegion: (region: string) => SetResult | null;
    isValidRoleArn: (arn: string) => boolean;
    clear: () => void;
    headers: () => Record<string, string>;
    parse: (text: string) => StoredCreds;
    listBuckets: () => Promise<BucketInfo[]>;
  };
};

// Minimal sessionStorage-like fake.
function fakeStorage(): StorageLike {
  const m = new Map<string, string>();
  return {
    getItem: (k) => (m.has(k) ? m.get(k)! : null),
    setItem: (k, v) => m.set(k, String(v)),
    removeItem: (k) => void m.delete(k),
  };
}

function freshStore(): void {
  Dial9Creds._setStorage(fakeStorage());
}

const H_AKID = "x-dial9-aws-access-key-id";
const H_SECRET = "x-dial9-aws-secret-access-key";
const H_TOKEN = "x-dial9-aws-session-token";
const H_REGION = "x-dial9-aws-region";
const H_ROLE_ARN = "x-dial9-aws-role-arn";

const VALID_ARN = "arn:aws:iam::123456789012:role/dial9-reader";

describe("Dial9Creds", () => {
  it("no credentials -> empty headers and has()=false", () => {
    freshStore();
    expect(Dial9Creds.has()).toBe(false);
    expect(Dial9Creds.headers()).toEqual({});
    expect(Dial9Creds.get()).toBeNull();
  });

  it("set() then headers() round-trips all fields", async () => {
    freshStore();
    // No fetch in Node -> set() stores as-is and skips region auto-detect.
    const result = await Dial9Creds.set({
      accessKeyId: "AKIA",
      secretAccessKey: "secret",
      sessionToken: "token",
      region: "us-west-2",
    });
    expect(result.ok).toBe(true);
    expect(Dial9Creds.has()).toBe(true);
    expect(Dial9Creds.headers()).toEqual({
      [H_AKID]: "AKIA",
      [H_SECRET]: "secret",
      [H_TOKEN]: "token",
      [H_REGION]: "us-west-2",
    });
  });

  it("headers() omits unset token and region", async () => {
    freshStore();
    await Dial9Creds.set({ accessKeyId: "AKIA", secretAccessKey: "secret" });
    const h = Dial9Creds.headers();
    expect(h).toEqual({ [H_AKID]: "AKIA", [H_SECRET]: "secret" });
    expect(H_TOKEN in h, "token header omitted").toBe(false);
    expect(H_REGION in h, "region header omitted").toBe(false);
  });

  it("set() trims whitespace and treats empty token/region as absent", async () => {
    freshStore();
    await Dial9Creds.set({
      accessKeyId: "  AKIA  ",
      secretAccessKey: " secret ",
      sessionToken: "   ",
      region: "",
    });
    const stored = Dial9Creds.get();
    expect(stored).not.toBeNull();
    expect(stored!.accessKeyId).toBe("AKIA");
    expect(stored!.secretAccessKey).toBe("secret");
    expect(stored!.sessionToken).toBeUndefined();
    expect(stored!.region).toBeUndefined();
  });

  it("set() rejects when a required field is missing", async () => {
    freshStore();
    await expect(
      Dial9Creds.set({ accessKeyId: "AKIA" }),
      "expected set() to throw on incomplete credentials",
    ).rejects.toThrow(/required/);
  });

  it("clear() removes stored credentials", async () => {
    freshStore();
    await Dial9Creds.set({ accessKeyId: "AKIA", secretAccessKey: "secret" });
    expect(Dial9Creds.has()).toBe(true);
    Dial9Creds.clear();
    expect(Dial9Creds.has()).toBe(false);
    expect(Dial9Creds.headers()).toEqual({});
  });

  // -- assume-role transport (setRoleArn / role-arn header) --

  it("setRoleArn() stores the ARN and emits the role-arn header", () => {
    freshStore();
    Dial9Creds.setRoleArn(VALID_ARN);
    expect(Dial9Creds.has()).toBe(true);
    expect(Dial9Creds.headers()).toEqual({ [H_ROLE_ARN]: VALID_ARN });
  });

  it("setRoleArn() carries an optional region alongside the ARN", () => {
    freshStore();
    Dial9Creds.setRoleArn(VALID_ARN, { region: "us-west-2" });
    expect(Dial9Creds.headers()).toEqual({
      [H_ROLE_ARN]: VALID_ARN,
      [H_REGION]: "us-west-2",
    });
  });

  it("setRoleArn() rejects a malformed ARN", () => {
    freshStore();
    expect(() => Dial9Creds.setRoleArn("not-an-arn")).toThrow(/invalid role ARN/);
    // A rejected ARN must not leave anything stored.
    expect(Dial9Creds.has()).toBe(false);
  });

  it("get(): a bag carrying both transports resolves to a single static kind", () => {
    // The server rejects a request carrying both transports (ConflictingCredentials),
    // so the store must resolve to exactly one. classify() is the single place that
    // invariant lives: a full key set is the more specific intent, so it wins and the
    // role ARN is dropped. Seed a store holding both directly (the writers never
    // produce this) to prove classify().
    const s = fakeStorage();
    Dial9Creds._setStorage(s);
    s.setItem(
      "dial9.aws-credentials",
      JSON.stringify({ accessKeyId: "AK", secretAccessKey: "SK", roleArn: VALID_ARN }),
    );
    const c = Dial9Creds.get();
    expect(c!.kind).toBe("static");
    expect("roleArn" in c!, "role ARN dropped when static keys present").toBe(false);
    const h = Dial9Creds.headers();
    expect(h[H_AKID]).toBe("AK");
    expect(h[H_SECRET]).toBe("SK");
    expect(H_ROLE_ARN in h, "role-arn header omitted when static keys present").toBe(
      false,
    );
  });

  it("get(): a legacy flat bag (no kind) is classified by the fields present", () => {
    // sessionStorage is tab-scoped, but a tab open across the upgrade to the
    // discriminated shape can hold a pre-kind bag. Static-only and role-only legacy
    // bags must still classify and emit the right headers.
    const s = fakeStorage();
    Dial9Creds._setStorage(s);
    s.setItem(
      "dial9.aws-credentials",
      JSON.stringify({ accessKeyId: "AK", secretAccessKey: "SK", region: "us-east-1" }),
    );
    expect(Dial9Creds.get()!.kind).toBe("static");
    expect(Dial9Creds.headers()).toEqual({
      [H_AKID]: "AK",
      [H_SECRET]: "SK",
      [H_REGION]: "us-east-1",
    });

    s.setItem("dial9.aws-credentials", JSON.stringify({ roleArn: VALID_ARN }));
    expect(Dial9Creds.get()!.kind).toBe("role");
    expect(Dial9Creds.headers()).toEqual({ [H_ROLE_ARN]: VALID_ARN });
  });

  // -- setRegion(): shape-agnostic region patch (both transports) --

  it("setRegion() pins the region on a static credential, preserving the keys", async () => {
    freshStore();
    await Dial9Creds.set({ accessKeyId: "AK", secretAccessKey: "SK", sessionToken: "TK" });
    Dial9Creds.setRegion("us-west-2");
    expect(Dial9Creds.headers()).toEqual({
      [H_AKID]: "AK",
      [H_SECRET]: "SK",
      [H_TOKEN]: "TK",
      [H_REGION]: "us-west-2",
    });
  });

  it("setRegion() pins the region on an assumed-role credential (the role path)", () => {
    // Region auto-detection persists the resolved region via setRegion. With a role
    // credential active this must keep the role transport - the old static-only
    // set({...stored, region}) would have thrown here.
    freshStore();
    Dial9Creds.setRoleArn(VALID_ARN);
    Dial9Creds.setRegion("eu-central-1");
    expect(Dial9Creds.headers()).toEqual({
      [H_ROLE_ARN]: VALID_ARN,
      [H_REGION]: "eu-central-1",
    });
    // Still a role credential - no static keys crept in.
    expect(Dial9Creds.get()!.kind).toBe("role");
  });

  it("setRegion() is a no-op when nothing is stored (ambient path)", () => {
    freshStore();
    expect(Dial9Creds.setRegion("us-east-1")).toBeNull();
    expect(Dial9Creds.has()).toBe(false);
    expect(Dial9Creds.headers()).toEqual({});
  });

  it("isValidRoleArn() mirrors the server's shape check", () => {
    expect(Dial9Creds.isValidRoleArn(VALID_ARN)).toBe(true);
    expect(
      Dial9Creds.isValidRoleArn("arn:aws:iam::123456789012:role/path/to/reader"),
    ).toBe(true);
    expect(Dial9Creds.isValidRoleArn("arn:aws-us-gov:iam::123456789012:role/r")).toBe(
      true,
    );
    // Rejections: wrong service, a region field, short account, wildcard, non-role.
    expect(Dial9Creds.isValidRoleArn("arn:aws:sts::123456789012:role/r")).toBe(false);
    expect(
      Dial9Creds.isValidRoleArn("arn:aws:iam:us-east-1:123456789012:role/r"),
    ).toBe(false);
    expect(Dial9Creds.isValidRoleArn("arn:aws:iam::12345:role/r")).toBe(false);
    expect(Dial9Creds.isValidRoleArn("arn:aws:iam::123456789012:role/*")).toBe(false);
    expect(Dial9Creds.isValidRoleArn("arn:aws:iam::123456789012:user/u")).toBe(false);
    expect(Dial9Creds.isValidRoleArn("")).toBe(false);
  });

  it("clear() removes a stored role ARN too", () => {
    freshStore();
    Dial9Creds.setRoleArn(VALID_ARN);
    expect(Dial9Creds.has()).toBe(true);
    Dial9Creds.clear();
    expect(Dial9Creds.has()).toBe(false);
    expect(Dial9Creds.headers()).toEqual({});
  });

  // -- parse(): pasted credential JSON --

  it("parse() extracts creds from an STS AssumeRole response", () => {
    // The real Isengard "copy credentials" blob (trimmed), including the stray
    // whitespace the user pasted after secretAccessKey.
    const blob = `{
      "sdkResponseMetadata": { "requestId": "859195cf" },
      "credentials": {
        "accessKeyId": "AKIAEXAMPLE",
        "secretAccessKey": "shhh-secret",                         "sessionToken": "tok123",
        "expiration": 1781920341000
      },
      "assumedRoleUser": {
        "arn": "arn:aws:sts::909186482670:assumed-role/ProfilingDataReader/rcoh-Isengard"
      }
    }`;
    const c = Dial9Creds.parse(blob);
    expect(c.accessKeyId).toBe("AKIAEXAMPLE");
    expect(c.secretAccessKey).toBe("shhh-secret");
    expect(c.sessionToken).toBe("tok123");
  });

  it("parse() accepts a flat credentials JSON object", () => {
    const c = Dial9Creds.parse(
      JSON.stringify({
        accessKeyId: "AK",
        secretAccessKey: "SK",
        sessionToken: "TK",
        region: "eu-west-1",
      }),
    );
    expect(c.accessKeyId).toBe("AK");
    expect(c.secretAccessKey).toBe("SK");
    expect(c.sessionToken).toBe("TK");
    expect(c.region).toBe("eu-west-1");
  });

  it("parse() tolerates capitalized STS key names", () => {
    const c = Dial9Creds.parse(
      JSON.stringify({
        Credentials: {
          AccessKeyId: "AK",
          SecretAccessKey: "SK",
          SessionToken: "TK",
        },
      }),
    );
    expect(c.accessKeyId).toBe("AK");
    expect(c.secretAccessKey).toBe("SK");
    expect(c.sessionToken).toBe("TK");
  });

  it("parse() throws on non-JSON", () => {
    expect(() => Dial9Creds.parse("not json at all")).toThrow(/not valid JSON/);
  });

  it("parse() throws when required fields are absent", () => {
    expect(() =>
      Dial9Creds.parse(JSON.stringify({ credentials: { expiration: 1 } })),
    ).toThrow(/could not find/);
  });

  it("listBuckets() returns each bucket's ListBuckets region", async () => {
    freshStore();
    await Dial9Creds.set({ accessKeyId: "AK", secretAccessKey: "SK" });
    let seen: { url: string; opts: { headers: Record<string, string> } } | undefined;
    vi.stubGlobal(
      "fetch",
      async (url: string, opts: { headers: Record<string, string> }) => {
        seen = { url, opts };
        return {
          ok: true,
          status: 200,
          async json() {
            return [
              { name: "dial9-cape-town", region: "af-south-1" },
              { name: "dial9-legacy", region: null },
            ];
          },
        };
      },
    );
    try {
      const buckets = await Dial9Creds.listBuckets();
      expect(buckets).toEqual([
        { name: "dial9-cape-town", region: "af-south-1" },
        { name: "dial9-legacy", region: null },
      ]);
      expect(seen!.url).toBe("/api/buckets");
      expect(seen!.opts.headers[H_AKID]).toBe("AK");
      expect(seen!.opts.headers[H_SECRET]).toBe("SK");
    } finally {
      vi.unstubAllGlobals();
    }
  });

  it("listBuckets() throws the server message on HTTP error", async () => {
    freshStore();
    await Dial9Creds.set({ accessKeyId: "AK", secretAccessKey: "SK" });
    vi.stubGlobal("fetch", async () => ({
      ok: false,
      status: 401,
      async text() {
        return "credentials rejected by S3";
      },
    }));
    try {
      const err = await Dial9Creds.listBuckets().then(
        () => null,
        (e: unknown) => e as Error,
      );
      expect(err, "expected listBuckets to throw on HTTP 401").not.toBeNull();
      expect(err!.message).toMatch(/401/);
      expect(err!.message).toMatch(/rejected/);
    } finally {
      vi.unstubAllGlobals();
    }
  });
});
