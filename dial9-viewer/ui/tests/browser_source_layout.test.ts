import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { describe, expect, it } from "vitest";

const indexHtml = readFileSync(
  fileURLToPath(new URL("../index.html", import.meta.url)),
  "utf8",
);

const match = indexHtml.match(
  /    function usesFlatSourceLayout\(config\) \{([\s\S]*?)\n    \}/,
);
if (!match) throw new Error("canonical index.html must define usesFlatSourceLayout()");

const usesFlatSourceLayout = new Function(
  `function usesFlatSourceLayout(config) {${match[1]}\n    }
   return usesFlatSourceLayout;`,
)() as (config: {
  source_layout?: string;
  supports_byo_credentials?: boolean;
}) => boolean;

describe("legacy browser source layout", () => {
  it("uses an explicit layout independently of credential support", () => {
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

  it("retains the old credential-based fallback", () => {
    expect(usesFlatSourceLayout({ supports_byo_credentials: false })).toBe(true);
    expect(usesFlatSourceLayout({ supports_byo_credentials: true })).toBe(false);
  });
});
