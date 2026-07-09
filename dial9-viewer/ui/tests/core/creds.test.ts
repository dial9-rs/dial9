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

interface StorageLike {
  getItem: (k: string) => string | null;
  setItem: (k: string, v: string) => void;
  removeItem: (k: string) => void;
}

const { Dial9Creds } = require("../../creds.js") as {
  Dial9Creds: {
    _setStorage: (s: StorageLike) => void;
    has: () => boolean;
    get: () => StoredCreds | null;
    set: (creds: Partial<StoredCreds>) => Promise<SetResult>;
    clear: () => void;
    headers: () => Record<string, string>;
    parse: (text: string) => StoredCreds;
    listBuckets: () => Promise<string[]>;
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

  it("listBuckets() sends cred headers and returns the list", async () => {
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
            return ["a", "dial9-traces", "b"];
          },
        };
      },
    );
    try {
      const names = await Dial9Creds.listBuckets();
      expect(names).toEqual(["a", "dial9-traces", "b"]);
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
