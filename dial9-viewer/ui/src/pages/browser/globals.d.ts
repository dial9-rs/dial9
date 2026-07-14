// Window globals the browser page consumes. These come from the two
// plain <script src> includes in new/index.html:
//
// - creds.js publishes window.Dial9Creds - the page's STABLE USERSCRIPT
//   contract, typed by src/types/creds.d.ts (type-only
//   import below; .d.ts files are exempt from the core-import boundary).
// - url_state.js publishes window.Dial9UrlState (#585 URL-state semantics,
//   shared verbatim with the legacy page).
// - ui-switch.js publishes window.D9UiSwitch (dual-UI switch).

import type { Dial9CredsApi } from "../../../creds.js";

/**
 * The URL-state shape url_state.js parses/serializes (its header comment is
 * the authority). All fields optional; parse() leaves invalid/absent fields
 * unset, serialize() omits defaults so a pristine page has a clean URL.
 */
export interface UrlStateFields {
  bucket?: string;
  /** Serialized as `aws_region` (matches the backend's query param). */
  region?: string;
  prefix?: string;
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

export interface D9UiSwitchApi {
  mount(opts?: { side?: "new" | "legacy"; page?: string | null }): void;
}

declare global {
  interface Window {
    /** Present when creds.js is loaded (always on this page). Optional in
     * the type because the legacy code guards every access with
     * `window.Dial9Creds ? ... : ...`, and this port preserves that. */
    Dial9Creds?: Dial9CredsApi;
    Dial9UrlState: Dial9UrlStateApi;
    D9UiSwitch?: D9UiSwitchApi;
  }
}
