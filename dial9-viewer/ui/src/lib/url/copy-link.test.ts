import { describe, expect, it } from "vitest";
import type { CopyLinkOptions } from "./copy-link.js";

describe("CopyLinkOptions", () => {
  it("requires beforeCopy to explicitly approve or reject the copy", () => {
    const options = { beforeCopy: () => true } satisfies CopyLinkOptions;
    expect(options.beforeCopy()).toBe(true);

    // @ts-expect-error A supplied hook must return an explicit decision.
    const invalid: CopyLinkOptions = { beforeCopy: () => {} };
    void invalid;
  });
});
