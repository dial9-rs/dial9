import { describe, expect, it } from "vitest";
import { infoMenuPosition } from "./info-menu.js";

describe("trace details menu placement", () => {
  it("keeps a wide menu on screen when the info button wraps to the left", () => {
    expect(infoMenuPosition({ right: 300, bottom: 110 }, { width: 600, height: 420 }, { width: 1400, height: 900 }))
      .toEqual({ left: 12, top: 114 });
  });
  it("keeps the right edge within the viewport", () => {
    expect(infoMenuPosition({ right: 1400, bottom: 60 }, { width: 600, height: 420 }, { width: 1400, height: 900 }))
      .toEqual({ left: 788, top: 64 });
  });
  it("keeps tall menus visible on short viewports", () => {
    expect(infoMenuPosition({ right: 300, bottom: 250 }, { width: 336, height: 276 }, { width: 360, height: 300 }))
      .toEqual({ left: 12, top: 12 });
  });
});
