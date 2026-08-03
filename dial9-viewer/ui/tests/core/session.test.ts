import { afterEach, describe, expect, it, vi } from "vitest";
import { createRequire } from "node:module";

const require = createRequire(import.meta.url);
const { Dial9Session } = require("../../session.js") as {
  Dial9Session: {
    get(): string | null;
    headers(base?: HeadersInit): Record<string, string>;
    fetch(input: RequestInfo | URL, init?: RequestInit): Promise<Response>;
    _setStorage(storage: StorageLike | null): void;
    _setRandomUuid(randomUuid: (() => string | null) | null): void;
  };
};

interface StorageLike {
  getItem(key: string): string | null;
  setItem(key: string, value: string): void;
}

const FIRST_ID = "123e4567-e89b-42d3-a456-426614174000";
const SECOND_ID = "987e6543-e21b-42d3-a456-426614174999";
const HEADER = "x-dial9-session-id";

function fakeStorage(initial?: string): StorageLike {
  const values = new Map<string, string>();
  if (initial !== undefined) values.set("dial9.session-id", initial);
  return {
    getItem: (key) => values.get(key) ?? null,
    setItem: (key, value) => values.set(key, value),
  };
}

function reset(storage: StorageLike = fakeStorage()): void {
  Dial9Session._setStorage(storage);
  Dial9Session._setRandomUuid(() => FIRST_ID);
}

afterEach(() => {
  vi.unstubAllGlobals();
  Dial9Session._setStorage(null);
  Dial9Session._setRandomUuid(null);
});

describe("Dial9Session", () => {
  it("generates one UUID per tab storage and reuses it", () => {
    const storage = fakeStorage();
    reset(storage);
    expect(Dial9Session.get()).toBe(FIRST_ID);

    Dial9Session._setRandomUuid(() => SECOND_ID);
    expect(Dial9Session.get()).toBe(FIRST_ID);
    expect(storage.getItem("dial9.session-id")).toBe(FIRST_ID);
  });

  it("replaces malformed stored content and never emits it", () => {
    reset(fakeStorage("not-a-uuid\r\ninjected"));
    expect(Dial9Session.headers({ accept: "application/json" })).toEqual({
      accept: "application/json",
      [HEADER]: FIRST_ID,
    });
  });

  it("adds the ID only to same-origin API fetches", async () => {
    reset();
    vi.stubGlobal("location", {
      origin: "https://dial9.example",
      href: "https://dial9.example/viewer.html",
    });
    const calls: {
      input: RequestInfo | URL;
      init: RequestInit | undefined;
    }[] = [];
    vi.stubGlobal(
      "fetch",
      vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
        calls.push({ input, init });
        return new Response(null, { status: 200 });
      }),
    );

    await Dial9Session.fetch("/api/config", {
      headers: { accept: "application/json", [HEADER]: SECOND_ID },
    });
    await Dial9Session.fetch("https://other.example/api/object");

    expect(new Headers(calls[0]!.init!.headers).get(HEADER)).toBe(FIRST_ID);
    expect(new Headers(calls[0]!.init!.headers).get("accept")).toBe(
      "application/json",
    );
    expect(new Headers(calls[1]!.init?.headers).has(HEADER)).toBe(false);
  });
});
