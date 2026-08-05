import { describe, expect, it } from "vitest";
import {
  bootScopeFromSearch,
  type ScopeBootCredentials,
  type ScopeLoadTarget,
} from "./scope-boot.js";
import type { ReparseRange } from "../../lib/trace/index.js";

const REPORTED_SEARCH =
  "?svc=shale&host=ip-10-2-118-83.us-west-2.compute.internal" +
  "&from=2026-07-20+23%3A09%3A58&segs=1" +
  "&s_bucket=cell1-prod-pdx-dial9-traces&s_region=us-west-2" +
  "&s_svc=shale&s_from=1784588998&s_to=1784589014" +
  "&s_host=ip-10-2-118-83.us-west-2.compute.internal";

const TRACE_KEY =
  "traces/2026-07-20/2309/shale/" +
  "ip-10-2-118-83.us-west-2.compute.internal/boot/1784588998-0.bin.gz";

describe("scope boot", () => {
  it("dispatches the reported s_* viewer URL through browse and into the loader", async () => {
    const events: string[] = [];
    const loads: Array<{
      urls: readonly string[];
      label: string;
      dataRange: ReparseRange | undefined;
    }> = [];
    const errors: string[] = [];
    let region = "us-east-1";
    let requested = "";

    const loadChrome: ScopeLoadTarget = {
      scopeLoading(label) {
        events.push(`loading:${label}`);
        return () => true;
      },
      loadUrls(urls, label, dataRange) {
        events.push("loadUrls");
        loads.push({ urls, label, dataRange });
      },
      scopeFailed() {
        events.push("failed");
      },
    };
    const creds: ScopeBootCredentials = {
      get: () => ({ region }),
      setRegion(next) {
        region = next;
      },
      setRoleArn() {
        /* no role in this URL */
      },
      has: () => true,
      headers: () => ({ "x-dial9-aws-region": region }),
    };

    const handled = await bootScopeFromSearch({
      search: REPORTED_SEARCH,
      hasInlineUrls: false,
      loadChrome,
      dataRange: { startNs: 1_784_588_999_000_000_000, endNs: 1_784_589_010_000_000_000 },
      onError: (message) => errors.push(message),
      creds,
      fetchJson: async (url) => {
        requested = url;
        return {
          objects: [
            {
              key: TRACE_KEY,
              size: 10,
              last_modified: "2026-07-20T23:10:14Z",
            },
          ],
        };
      },
    });

    expect(handled).toBe(true);
    expect(region).toBe("us-west-2");
    expect(requested).toBe(
      "/api/browse?bucket=cell1-prod-pdx-dial9-traces&service=shale" +
        "&from=1784588998&to=1784589014",
    );
    expect(events).toEqual(["loading:Loading trace selection…", "loadUrls"]);
    expect(errors).toEqual([]);
    expect(loads).toEqual([
      {
        urls: [
          "/api/object?bucket=cell1-prod-pdx-dial9-traces&key=" +
            encodeURIComponent(TRACE_KEY),
        ],
        label: "Loading trace...",
        dataRange: {
          startNs: 1_784_588_999_000_000_000,
          endNs: 1_784_589_010_000_000_000,
        },
      },
    ]);
  });

  it("restores the scope's role ARN into creds so a fresh-session link has an identity", async () => {
    // The bug: a shared viewer link opened in a session with NO stored creds
    // carries the bucket + region + role in its s_* scope, but the role was
    // never restored, so /api/browse 401'd. Assert the role is folded back in.
    const roleCalls: Array<{ arn: string; region?: string }> = [];
    let region = "";
    const creds: ScopeBootCredentials = {
      get: () => ({ region }),
      setRegion(next) {
        region = next;
      },
      setRoleArn(arn, opts) {
        roleCalls.push({ arn, region: opts?.region });
        if (opts?.region) region = opts.region;
      },
      has: () => false, // fresh session: no stored creds
      headers: () => ({}),
    };

    const search =
      REPORTED_SEARCH +
      "&s_role_arn=" +
      encodeURIComponent("arn:aws:iam::123456789012:role/Dial9TraceReader");

    const handled = await bootScopeFromSearch({
      search,
      hasInlineUrls: false,
      loadChrome: {
        scopeLoading: () => () => true,
        loadUrls: () => {},
        scopeFailed: () => {},
      },
      onError: () => {},
      creds,
      fetchJson: async () => ({
        objects: [
          { key: TRACE_KEY, size: 10, last_modified: "2026-07-20T23:10:14Z" },
        ],
      }),
    });

    expect(handled).toBe(true);
    expect(roleCalls).toEqual([
      {
        arn: "arn:aws:iam::123456789012:role/Dial9TraceReader",
        region: "us-west-2",
      },
    ]);
  });

  it("does not restart loading when the scope is cancelled during browse", async () => {
    const events: string[] = [];
    let current = true;
    let finishBrowse: ((value: unknown) => void) | undefined;
    const browse = new Promise<unknown>((resolve) => {
      finishBrowse = resolve;
    });
    const loadChrome: ScopeLoadTarget = {
      scopeLoading() {
        events.push("loading");
        return () => current;
      },
      loadUrls() {
        events.push("loadUrls");
      },
      scopeFailed() {
        events.push("failed");
      },
    };

    const boot = bootScopeFromSearch({
      search: REPORTED_SEARCH,
      hasInlineUrls: false,
      loadChrome,
      onError: (message) => events.push(`error:${message}`),
      fetchJson: () => browse,
    });
    expect(events).toEqual(["loading"]);

    current = false; // Escape/cancel invalidates the controller generation.
    finishBrowse?.({
      objects: [
        {
          key: TRACE_KEY,
          size: 10,
          last_modified: "2026-07-20T23:10:14Z",
        },
      ],
    });
    await boot;

    expect(events).toEqual(["loading"]);
  });
});
