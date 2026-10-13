// The "back to the trace browser" link every leaf page carries. Leaf pages open
// in a new tab, so history.back() has nowhere to go; instead the link rebuilds
// the browser URL from the leaf page's own scope (bucket, identity, prefix,
// service, time window, host selection) so the browser reopens on the same
// selection.

import { parseDiff } from "../canvas/flamegraph_diff.js";
import { encodeScope, readScope } from "../trace/trace_scope.js";

const SOURCE_PARAMS = ["bucket", "aws_region", "aws_role_arn", "credential_mode"] as const;

interface BrowserScope {
  source: Record<(typeof SOURCE_PARAMS)[number], string>;
  prefix: string;
  service: string;
  hosts: string[];
  window: { from: number; to: number } | null;
}

function nsToSeconds(value: string | null, round: (n: number) => number): number | null {
  if (value === null || value === "") return null;
  const ns = Number(value);
  return Number.isFinite(ns) ? round(ns / 1e9) : null;
}

function aggregationScope(params: URLSearchParams): BrowserScope {
  const from = nsToSeconds(params.get("start_ns"), Math.floor);
  const to = nsToSeconds(params.get("end_ns"), Math.ceil);
  return {
    source: {
      bucket: params.get("bucket") ?? "",
      aws_region: params.get("aws_region") ?? "",
      aws_role_arn: params.get("aws_role_arn") ?? "",
      credential_mode: params.get("credential_mode") ?? "",
    },
    prefix: params.get("prefix") ?? "",
    service: params.get("service") ?? "",
    hosts: params.getAll("host"),
    window: from !== null && to !== null ? { from, to } : null,
  };
}

function leafScope(params: URLSearchParams): BrowserScope {
  const scope = readScope(params);
  if (scope !== null) {
    return {
      source: {
        bucket: scope.bucket,
        aws_region: scope.region,
        aws_role_arn: scope.roleArn,
        credential_mode: scope.credentialMode,
      },
      prefix: scope.prefix,
      service: scope.service,
      hosts: scope.hosts,
      window: { from: scope.from, to: scope.to },
    };
  }
  return aggregationScope(parseDiff(params)?.a ?? params);
}

/**
 * The trace-browser URL that returns to a leaf page's scope.
 *
 * Accepts the viewer's `s_*` scope, the aggregation vocabulary, or a diff
 * link (side A). A page without a bucket scope links to the browser root.
 */
export function browserHref(search: string): string {
  const scope = leafScope(new URLSearchParams(search));
  if (scope.source.bucket === "") return "/";
  const params = new URLSearchParams();
  for (const name of SOURCE_PARAMS) {
    if (scope.source[name] !== "") params.set(name, scope.source[name]);
  }
  if (scope.prefix !== "") params.set("prefix", scope.prefix);
  if (scope.service !== "") params.set("service", scope.service);
  if (scope.window === null) return "/?" + params.toString();
  params.set("from", String(scope.window.from));
  params.set("to", String(scope.window.to));
  const { query } = encodeScope(params, {
    bucket: "",
    region: "",
    roleArn: "",
    credentialMode: "",
    prefix: "",
    service: "",
    hosts: scope.hosts,
    from: scope.window.from,
    to: scope.window.to,
  });
  return "/?" + query;
}

export const BROWSER_LINK_LABEL = "←";
export const BROWSER_LINK_TITLE = "Back to the trace browser";

/** Prepend the browser link to a page header. */
export function mountBrowserLink(container: HTMLElement, search: string): HTMLAnchorElement {
  const link = container.ownerDocument.createElement("a");
  link.className = "d9-browser-link";
  link.href = browserHref(search);
  link.textContent = BROWSER_LINK_LABEL;
  link.title = BROWSER_LINK_TITLE;
  link.setAttribute("aria-label", BROWSER_LINK_TITLE);
  link.style.cssText =
    "color:#8b9dff;text-decoration:none;white-space:nowrap;flex-shrink:0;" +
    "font-size:1.2rem;line-height:1;";
  container.prepend(link);
  return link;
}
