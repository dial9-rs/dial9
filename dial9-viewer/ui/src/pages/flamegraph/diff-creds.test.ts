import { describe, expect, it } from "vitest";
import { sideBCheckRequest, sideBHeaders } from "./diff-creds.js";

const CREDS = { accessKeyId: "AK", secretAccessKey: "SK", sessionToken: "TK" };

describe("sideBHeaders", () => {
  const TAB = { "x-dial9-aws-role-arn": "arn:aws:iam::123456789012:role/A", "x-dial9-aws-region": "us-east-1" };

  it.each([
    { name: "pins tab credentials to B's linked region", scope: "bucket=bb&aws_region=eu-west-1", creds: null, region: "eu-west-1" },
    { name: "uses pasted B credentials' own region", scope: "bucket=bb&aws_region=eu-west-1", creds: { ...CREDS, region: "ap-south-1" }, region: "ap-south-1" },
  ])("$name", ({ scope, creds, region }) => {
    expect(sideBHeaders(new URLSearchParams(scope), creds, TAB)["x-dial9-aws-region"]).toBe(region);
  });
});

describe("sideBCheckRequest", () => {
  it.each([
    {
      name: "scopes the check to B's prefix and region",
      scope: "bucket=bb&aws_region=us-west-2&prefix=traces/svc&service=svc",
      creds: CREDS,
      url: "/api/credentials/check?bucket=bb&prefix=traces%2Fsvc",
      region: "us-west-2",
    },
    {
      name: "keeps a region named by the pasted credentials",
      scope: "bucket=bb&aws_region=us-west-2&prefix=traces",
      creds: { ...CREDS, region: "eu-west-1" },
      url: "/api/credentials/check?bucket=bb&prefix=traces",
      region: "eu-west-1",
    },
    {
      name: "keeps the bucket-level check without a prefix",
      scope: "bucket=bb",
      creds: CREDS,
      url: "/api/credentials/check?bucket=bb",
      region: undefined,
    },
  ])("$name", ({ scope, creds, url, region }) => {
    const request = sideBCheckRequest(new URLSearchParams(scope), creds);

    expect(request.url).toBe(url);
    expect(request.headers["x-dial9-aws-region"]).toBe(region);
    expect(request.headers["x-dial9-aws-access-key-id"]).toBe("AK");
    expect(request.headers["x-dial9-aws-session-token"]).toBe("TK");
  });
});
