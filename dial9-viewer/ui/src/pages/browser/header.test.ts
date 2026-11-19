import { readFile } from "node:fs/promises";
import { describe, expect, it } from "vitest";
import { configureHomeLink } from "./header.js";

class FakeAnchor {
  href = "";
  hidden = true;
}

describe("browser home link", () => {
  it("starts and remains hidden without a configured path", async () => {
    const html = await readFile(new URL("../../../index.html", import.meta.url), "utf8");
    expect(html).toContain('id="home-link" class="home-link" hidden');

    const link = new FakeAnchor();
    configureHomeLink(link as unknown as HTMLAnchorElement, undefined);
    expect(link).toEqual({ href: "", hidden: true });
  });

  it("reveals the configured same-origin path", () => {
    const link = new FakeAnchor();
    configureHomeLink(link as unknown as HTMLAnchorElement, "/landing");
    expect(link).toEqual({ href: "/landing", hidden: false });
  });
});
