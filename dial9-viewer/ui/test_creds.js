#!/usr/bin/env node
"use strict";

// Tests for creds.js — the bring-your-own-credentials store and its stable
// scripting API (window.Dial9Creds.set/get/clear/headers). Runs in Node with an
// injected fake storage backend.

const { test, testAsync, assert, summarize } = require("./test_harness.js");
const { Dial9Creds } = require("./creds.js");

// Minimal sessionStorage-like fake.
function fakeStorage() {
  const m = new Map();
  return {
    getItem: (k) => (m.has(k) ? m.get(k) : null),
    setItem: (k, v) => m.set(k, String(v)),
    removeItem: (k) => m.delete(k),
  };
}

function freshStore() {
  Dial9Creds._setStorage(fakeStorage());
}

const H_AKID = "x-dial9-aws-access-key-id";
const H_SECRET = "x-dial9-aws-secret-access-key";
const H_TOKEN = "x-dial9-aws-session-token";
const H_REGION = "x-dial9-aws-region";
const H_ROLE_ARN = "x-dial9-aws-role-arn";

const VALID_ARN = "arn:aws:iam::123456789012:role/dial9-reader";

async function main() {

test("no credentials → empty headers and has()=false", () => {
  freshStore();
  assert.strictEqual(Dial9Creds.has(), false);
  assert.deepStrictEqual(Dial9Creds.headers(), {});
  assert.strictEqual(Dial9Creds.get(), null);
});

await testAsync("set() then headers() round-trips all fields", async () => {
  freshStore();
  // No fetch in Node → set() stores as-is and skips region auto-detect.
  const result = await Dial9Creds.set({
    accessKeyId: "AKIA",
    secretAccessKey: "secret",
    sessionToken: "token",
    region: "us-west-2",
  });
  assert.strictEqual(result.ok, true);
  assert.strictEqual(Dial9Creds.has(), true);
  assert.deepStrictEqual(Dial9Creds.headers(), {
    [H_AKID]: "AKIA",
    [H_SECRET]: "secret",
    [H_TOKEN]: "token",
    [H_REGION]: "us-west-2",
  });
});

await testAsync("headers() omits unset token and region", async () => {
  freshStore();
  await Dial9Creds.set({ accessKeyId: "AKIA", secretAccessKey: "secret" });
  const h = Dial9Creds.headers();
  assert.deepStrictEqual(h, { [H_AKID]: "AKIA", [H_SECRET]: "secret" });
  assert.ok(!(H_TOKEN in h), "token header omitted");
  assert.ok(!(H_REGION in h), "region header omitted");
});

await testAsync("set() trims whitespace and treats empty token/region as absent", async () => {
  freshStore();
  await Dial9Creds.set({
    accessKeyId: "  AKIA  ",
    secretAccessKey: " secret ",
    sessionToken: "   ",
    region: "",
  });
  const stored = Dial9Creds.get();
  assert.strictEqual(stored.accessKeyId, "AKIA");
  assert.strictEqual(stored.secretAccessKey, "secret");
  assert.strictEqual(stored.sessionToken, undefined);
  assert.strictEqual(stored.region, undefined);
});

await testAsync("set() rejects when a required field is missing", async () => {
  freshStore();
  let threw = false;
  try {
    await Dial9Creds.set({ accessKeyId: "AKIA" });
  } catch (e) {
    threw = true;
    assert.ok(/required/.test(e.message), `message mentions required: ${e.message}`);
  }
  assert.ok(threw, "expected set() to throw on incomplete credentials");
});

await testAsync("clear() removes stored credentials", async () => {
  freshStore();
  await Dial9Creds.set({ accessKeyId: "AKIA", secretAccessKey: "secret" });
  assert.strictEqual(Dial9Creds.has(), true);
  Dial9Creds.clear();
  assert.strictEqual(Dial9Creds.has(), false);
  assert.deepStrictEqual(Dial9Creds.headers(), {});
});

// ── assume-role transport (setRoleArn / role-arn header) ──

test("setRoleArn() stores the ARN and emits the role-arn header", () => {
  freshStore();
  Dial9Creds.setRoleArn(VALID_ARN);
  assert.strictEqual(Dial9Creds.has(), true);
  assert.deepStrictEqual(Dial9Creds.headers(), { [H_ROLE_ARN]: VALID_ARN });
});

test("setRoleArn() carries an optional region alongside the ARN", () => {
  freshStore();
  Dial9Creds.setRoleArn(VALID_ARN, { region: "us-west-2" });
  assert.deepStrictEqual(Dial9Creds.headers(), {
    [H_ROLE_ARN]: VALID_ARN,
    [H_REGION]: "us-west-2",
  });
});

test("setRoleArn() rejects a malformed ARN", () => {
  freshStore();
  assert.throws(() => Dial9Creds.setRoleArn("not-an-arn"), /invalid role ARN/);
  // A rejected ARN must not leave anything stored.
  assert.strictEqual(Dial9Creds.has(), false);
});

test("get(): a bag carrying both transports resolves to a single static kind", () => {
  // The server rejects a request carrying both transports
  // (ConflictingCredentials), so the store must resolve to exactly one. classify()
  // is the single place that invariant lives: a full key set is the more specific
  // intent, so it wins and the role ARN is dropped. Seed a store holding both
  // directly (the writers never produce this) to prove classify().
  const s = fakeStorage();
  Dial9Creds._setStorage(s);
  s.setItem(
    "dial9.aws-credentials",
    JSON.stringify({ accessKeyId: "AK", secretAccessKey: "SK", roleArn: VALID_ARN })
  );
  const c = Dial9Creds.get();
  assert.strictEqual(c.kind, "static");
  assert.ok(!("roleArn" in c), "role ARN dropped when static keys present");
  const h = Dial9Creds.headers();
  assert.strictEqual(h[H_AKID], "AK");
  assert.strictEqual(h[H_SECRET], "SK");
  assert.ok(!(H_ROLE_ARN in h), "role-arn header omitted when static keys present");
});

test("get(): a legacy flat bag (no kind) is classified by the fields present", () => {
  // sessionStorage is tab-scoped, but a tab open across the upgrade to the
  // discriminated shape can hold a pre-kind bag. Static-only and role-only legacy
  // bags must still classify and emit the right headers.
  const s = fakeStorage();
  Dial9Creds._setStorage(s);
  s.setItem(
    "dial9.aws-credentials",
    JSON.stringify({ accessKeyId: "AK", secretAccessKey: "SK", region: "us-east-1" })
  );
  assert.strictEqual(Dial9Creds.get().kind, "static");
  assert.deepStrictEqual(Dial9Creds.headers(), {
    [H_AKID]: "AK",
    [H_SECRET]: "SK",
    [H_REGION]: "us-east-1",
  });

  s.setItem("dial9.aws-credentials", JSON.stringify({ roleArn: VALID_ARN }));
  assert.strictEqual(Dial9Creds.get().kind, "role");
  assert.deepStrictEqual(Dial9Creds.headers(), { [H_ROLE_ARN]: VALID_ARN });
});

// ── setRegion(): shape-agnostic region patch (both transports) ──

test("setRegion() pins the region on a static credential, preserving the keys", () => {
  freshStore();
  Dial9Creds.set({ accessKeyId: "AK", secretAccessKey: "SK", sessionToken: "TK" });
  Dial9Creds.setRegion("us-west-2");
  assert.deepStrictEqual(Dial9Creds.headers(), {
    [H_AKID]: "AK",
    [H_SECRET]: "SK",
    [H_TOKEN]: "TK",
    [H_REGION]: "us-west-2",
  });
});

test("setRegion() pins the region on an assumed-role credential (the role path)", () => {
  // Region auto-detection persists the resolved region via setRegion. With a role
  // credential active this must keep the role transport — the old static-only
  // set({...stored, region}) would have thrown here.
  freshStore();
  Dial9Creds.setRoleArn(VALID_ARN);
  Dial9Creds.setRegion("eu-central-1");
  assert.deepStrictEqual(Dial9Creds.headers(), {
    [H_ROLE_ARN]: VALID_ARN,
    [H_REGION]: "eu-central-1",
  });
  // Still a role credential — no static keys crept in.
  assert.strictEqual(Dial9Creds.get().kind, "role");
});

test("setRegion() is a no-op when nothing is stored (ambient path)", () => {
  freshStore();
  assert.strictEqual(Dial9Creds.setRegion("us-east-1"), null);
  assert.strictEqual(Dial9Creds.has(), false);
  assert.deepStrictEqual(Dial9Creds.headers(), {});
});

test("isValidRoleArn() mirrors the server's shape check", () => {
  assert.ok(Dial9Creds.isValidRoleArn(VALID_ARN));
  assert.ok(Dial9Creds.isValidRoleArn("arn:aws:iam::123456789012:role/path/to/reader"));
  assert.ok(Dial9Creds.isValidRoleArn("arn:aws-us-gov:iam::123456789012:role/r"));
  // Rejections: wrong service, a region field, short account, wildcard, non-role.
  assert.ok(!Dial9Creds.isValidRoleArn("arn:aws:sts::123456789012:role/r"));
  assert.ok(!Dial9Creds.isValidRoleArn("arn:aws:iam:us-east-1:123456789012:role/r"));
  assert.ok(!Dial9Creds.isValidRoleArn("arn:aws:iam::12345:role/r"));
  assert.ok(!Dial9Creds.isValidRoleArn("arn:aws:iam::123456789012:role/*"));
  assert.ok(!Dial9Creds.isValidRoleArn("arn:aws:iam::123456789012:user/u"));
  assert.ok(!Dial9Creds.isValidRoleArn(""));
});

test("clear() removes a stored role ARN too", () => {
  freshStore();
  Dial9Creds.setRoleArn(VALID_ARN);
  assert.strictEqual(Dial9Creds.has(), true);
  Dial9Creds.clear();
  assert.strictEqual(Dial9Creds.has(), false);
  assert.deepStrictEqual(Dial9Creds.headers(), {});
});

// ── parse(): pasted credential JSON ──

test("parse() extracts creds from an STS AssumeRole response", () => {
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
  assert.strictEqual(c.accessKeyId, "AKIAEXAMPLE");
  assert.strictEqual(c.secretAccessKey, "shhh-secret");
  assert.strictEqual(c.sessionToken, "tok123");
});

test("parse() accepts a flat credentials JSON object", () => {
  const c = Dial9Creds.parse(
    JSON.stringify({
      accessKeyId: "AK",
      secretAccessKey: "SK",
      sessionToken: "TK",
      region: "eu-west-1",
    })
  );
  assert.strictEqual(c.accessKeyId, "AK");
  assert.strictEqual(c.secretAccessKey, "SK");
  assert.strictEqual(c.sessionToken, "TK");
  assert.strictEqual(c.region, "eu-west-1");
});

test("parse() tolerates capitalized STS key names", () => {
  const c = Dial9Creds.parse(
    JSON.stringify({
      Credentials: { AccessKeyId: "AK", SecretAccessKey: "SK", SessionToken: "TK" },
    })
  );
  assert.strictEqual(c.accessKeyId, "AK");
  assert.strictEqual(c.secretAccessKey, "SK");
  assert.strictEqual(c.sessionToken, "TK");
});

test("parse() throws on non-JSON", () => {
  assert.throws(() => Dial9Creds.parse("not json at all"), /not valid JSON/);
});

test("parse() throws when required fields are absent", () => {
  assert.throws(
    () => Dial9Creds.parse(JSON.stringify({ credentials: { expiration: 1 } })),
    /could not find/
  );
});

await testAsync("listBuckets() sends cred headers and returns the list", async () => {
  freshStore();
  await Dial9Creds.set({ accessKeyId: "AK", secretAccessKey: "SK" });
  let seen;
  const original = global.fetch;
  global.fetch = async (url, opts) => {
    seen = { url, opts };
    return { ok: true, status: 200, async json() { return ["a", "dial9-traces", "b"]; } };
  };
  try {
    const names = await Dial9Creds.listBuckets();
    assert.deepStrictEqual(names, ["a", "dial9-traces", "b"]);
    assert.strictEqual(seen.url, "/api/buckets");
    assert.strictEqual(seen.opts.headers[H_AKID], "AK");
    assert.strictEqual(seen.opts.headers[H_SECRET], "SK");
  } finally {
    global.fetch = original;
  }
});

await testAsync("listBuckets() throws the server message on HTTP error", async () => {
  freshStore();
  await Dial9Creds.set({ accessKeyId: "AK", secretAccessKey: "SK" });
  const original = global.fetch;
  global.fetch = async () => ({
    ok: false,
    status: 401,
    async text() { return "credentials rejected by S3"; },
  });
  try {
    let threw = false;
    try {
      await Dial9Creds.listBuckets();
    } catch (e) {
      threw = true;
      assert.ok(/401/.test(e.message) && /rejected/.test(e.message), e.message);
    }
    assert.ok(threw, "expected listBuckets to throw on HTTP 401");
  } finally {
    global.fetch = original;
  }
});

summarize();

}

main();
