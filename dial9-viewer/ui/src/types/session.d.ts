declare module "*/session.js" {
  export interface SessionStorageLike {
    getItem(key: string): string | null;
    setItem(key: string, value: string): void;
  }

  export interface Dial9SessionApi {
    /** This browser tab's opaque UUID, or null when secure generation fails. */
    get(): string | null;
    /** Copy `base` and add this tab's x-dial9-session-id header. */
    headers(base?: HeadersInit): Record<string, string>;
    /** Attach the session header only to same-origin /api requests. */
    fetch(input: RequestInfo | URL, init?: RequestInit): Promise<Response>;
    /** Test seam: inject sessionStorage-compatible storage. */
    _setStorage(storage: SessionStorageLike | null): void;
    /** Test seam: inject UUID generation. */
    _setRandomUuid(randomUuid: (() => string | null) | null): void;
  }

  export const Dial9Session: Dial9SessionApi;
}
