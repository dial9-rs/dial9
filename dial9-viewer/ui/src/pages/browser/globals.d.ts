// Window globals the browser page consumes. These come from the two plain
// <script src> includes in index.html:
//
// - creds.js publishes window.Dial9Creds - the page's STABLE USERSCRIPT
//   contract, typed by src/types/creds.d.ts (type-only
//   import below; .d.ts files are exempt from the core-import boundary).
// - url_state.js publishes window.Dial9UrlState (#585 URL-state semantics).

import type { Dial9CredsUserscriptApi } from "../../../creds.js";

/**
 * The URL-state shape url_state.js parses/serializes (its header comment is
 * the authority). All fields optional; parse() leaves invalid/absent fields
 * unset, serialize() omits defaults so a pristine page has a clean URL.
 */
export interface UrlStateFields {
  bucket?: string;
  /** Serialized as `aws_region` (matches the backend's query param). */
  region?: string;
  /** Explicit frontend credential source. */
  credentialMode?: "ambient" | "literal" | "role";
  /** IAM role the server should assume; serialized only in role mode. */
  roleArn?: string;
  prefix?: string;
  /** Optional service filter for Browse mode. */
  service?: string;
  tab?: "browse" | "raw";
  tz?: "utc" | "local";
  /** Relative quick-range window in hours; mutually exclusive with from/to. */
  last?: number;
  /** Precise window start, epoch seconds. */
  from?: number;
  /** Precise window end, epoch seconds. */
  to?: number;
  /** Raw-search prefix query. */
  q?: string;
}

export interface Dial9UrlStateApi {
  parse(search: string): UrlStateFields;
  serialize(state: UrlStateFields): string;
}

declare global {
  interface Window {
    /** Exact stable facade used by the private credential-loader userscript. */
    Dial9Creds?: Dial9CredsUserscriptApi;
    Dial9UrlState: Dial9UrlStateApi;
  }
}
