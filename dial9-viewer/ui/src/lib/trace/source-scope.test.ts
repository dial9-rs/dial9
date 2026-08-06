// SourceScope owns the bucket+region+role identity a shareable link carries and
// the header-vs-query transport asymmetry between region and the role. These
// tests pin both: the two-vocabulary round trips (incl. roleArn), the
// applyToCreds fold, and — the crux of the whole refactor — that a REQUEST URL
// never carries the role (header-only; the server's ConflictingCredentials 400)
// while STILL carrying the region (server tolerates both; ambient cross-region
// reads region only from the query param).
//
// Imports the module directly (not the lib/trace barrel) so the node test env
// does not pull the barrel's parser-loading modules.

import { describe, it, expect } from "vitest";
import {
  applyToCreds,
  makeSourceScope,
  readNamespacedSourceScope,
  readPlainSourceScope,
  writeNamespacedParams,
  writeRequestParams,
  writeShareableParams,
  type SourceScope,
  type SourceScopeCredentials,
} from "./source-scope.js";

const ROLE = "arn:aws:iam::123456789012:role/Dial9TraceReader";

const FULL: SourceScope = {
  bucket: "cell1-prod-pdx-dial9-traces",
  region: "us-west-2",
  roleArn: ROLE,
};

describe("makeSourceScope", () => {
  it("normalizes null / undefined / empty to ''", () => {
    expect(makeSourceScope(null, undefined, "")).toEqual({
      bucket: "",
      region: "",
      roleArn: "",
    });
  });
});

describe("vocabulary round trips (incl. roleArn)", () => {
  it("namespaced s_* round-trips the full identity", () => {
    const p = new URLSearchParams();
    writeNamespacedParams(p, FULL);
    expect(p.get("s_bucket")).toBe(FULL.bucket);
    expect(p.get("s_region")).toBe("us-west-2");
    expect(p.get("s_role_arn")).toBe(ROLE);
    expect(readNamespacedSourceScope(p)).toEqual(FULL);
  });

  it("plain vocabulary round-trips the full identity", () => {
    const p = new URLSearchParams();
    writeShareableParams(p, FULL);
    expect(p.get("bucket")).toBe(FULL.bucket);
    expect(p.get("aws_region")).toBe("us-west-2");
    expect(p.get("aws_role_arn")).toBe(ROLE);
    expect(readPlainSourceScope(p)).toEqual(FULL);
  });

  it("an empty identity serializes nothing and reads back empty", () => {
    const p = new URLSearchParams();
    writeShareableParams(p, makeSourceScope("", "", ""));
    expect(p.toString()).toBe("");
    expect(readPlainSourceScope(p)).toEqual({ bucket: "", region: "", roleArn: "" });
  });
});

describe("the single-transport rule: role header-only, region both", () => {
  it("writeRequestParams carries region but NEVER the role", () => {
    // The invariant: a page that folds the role into creds (a header) must not
    // also stamp aws_role_arn on its /api/* request — the server rejects a role
    // on both header and query with ConflictingCredentials (HTTP 400).
    const p = new URLSearchParams();
    writeRequestParams(p, FULL);
    expect(p.get("bucket")).toBe(FULL.bucket);
    expect(p.get("aws_region")).toBe("us-west-2");
    expect(p.has("aws_role_arn")).toBe(false);
  });

  it("writeShareableParams DOES carry the role (opened tab restores it at boot)", () => {
    const p = new URLSearchParams();
    writeShareableParams(p, FULL);
    expect(p.get("aws_role_arn")).toBe(ROLE);
  });
});

describe("applyToCreds", () => {
  function fakeCreds(initial: { region?: string } | null): {
    creds: SourceScopeCredentials;
    calls: string[];
    stored: { region?: string } | null;
  } {
    let stored = initial;
    const calls: string[] = [];
    const creds: SourceScopeCredentials = {
      get: () => stored,
      setRoleArn(arn, opts) {
        calls.push(`setRoleArn:${arn}:${opts?.region ?? ""}`);
        stored = opts?.region === undefined ? {} : { region: opts.region };
        return stored;
      },
      setRegion(region) {
        calls.push(`setRegion:${region}`);
        if (stored) stored = { ...stored, region };
        return stored;
      },
    };
    return {
      creds,
      calls,
      get stored() {
        return stored;
      },
    };
  }

  it("a present role is stored unconditionally, carrying its region", () => {
    const f = fakeCreds(null);
    applyToCreds(FULL, f.creds);
    expect(f.calls).toEqual([`setRoleArn:${ROLE}:us-west-2`]);
    expect(f.stored).toEqual({ region: "us-west-2" });
  });

  it("region-only with a stored credential patches the region as a header", () => {
    const f = fakeCreds({ region: "us-east-1" });
    applyToCreds(makeSourceScope("b", "us-west-2", ""), f.creds);
    expect(f.calls).toEqual(["setRegion:us-west-2"]);
    expect(f.stored).toEqual({ region: "us-west-2" });
  });

  it("region-only with an EMPTY store is a no-op (setRegion cannot make a credential)", () => {
    // The asymmetry that forces region onto the request URL: with nothing
    // stored, region cannot become a header, so applyToCreds leaves the store
    // untouched and the region must ride the /api/* query instead.
    const f = fakeCreds(null);
    applyToCreds(makeSourceScope("b", "us-west-2", ""), f.creds);
    expect(f.calls).toEqual([]);
    expect(f.stored).toBe(null);
  });

  it("does not re-patch a region that already matches", () => {
    const f = fakeCreds({ region: "us-west-2" });
    applyToCreds(makeSourceScope("b", "us-west-2", ""), f.creds);
    expect(f.calls).toEqual([]);
  });

  it("an empty identity touches nothing", () => {
    const f = fakeCreds({ region: "us-east-1" });
    applyToCreds(makeSourceScope("", "", ""), f.creds);
    expect(f.calls).toEqual([]);
  });
});
