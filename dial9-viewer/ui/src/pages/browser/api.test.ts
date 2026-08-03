import { describe, expect, it } from "vitest";

import { usesFlatSourceLayout } from "./api.js";

describe("usesFlatSourceLayout", () => {
  it("uses the explicit source layout independently of credentials", () => {
    expect(
      usesFlatSourceLayout({
        source_layout: "time-partitioned",
        supports_byo_credentials: false,
      }),
    ).toBe(false);
    expect(
      usesFlatSourceLayout({
        source_layout: "flat",
        supports_byo_credentials: true,
      }),
    ).toBe(true);
  });

  it("falls back to credential support for older servers", () => {
    expect(usesFlatSourceLayout({ supports_byo_credentials: false })).toBe(true);
    expect(usesFlatSourceLayout({ supports_byo_credentials: true })).toBe(false);
  });
});
