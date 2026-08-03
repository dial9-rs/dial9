import { describe, expect, it } from "vitest";
import { buildRegionFlamegraphPopoutUrl } from "./region-popout.js";

describe("region flamegraph pop-out", () => {
  it("serializes canonical store zoom and inspect state, not stale alternate state", () => {
    const href = buildRegionFlamegraphPopoutUrl(
      "?trace=traces%2Fa.bin&trace=%2Fapi%2Fobject%3Fkey%3Db.bin" +
        "&worker-zoom=stale%09widget",
      { startNs: 100.4, endNs: 900.6 },
      {
        regionWorkerZoom: ["canonical", "poll"],
        regionOffworkerZoom: ["off", "wait"],
        regionInspectFocus: "tokio::runtime::task::harness::poll_future",
      },
      true,
    );

    expect(href).not.toBeNull();
    const url = new URL(href!, "https://viewer.example/new/viewer.html");
    expect(url.pathname).toBe("/new/flamegraph.html");
    expect(url.searchParams.getAll("trace")).toEqual([
      "traces/a.bin",
      "/api/object?key=b.bin",
    ]);
    expect(url.searchParams.get("start")).toBe("100");
    expect(url.searchParams.get("end")).toBe("901");
    expect(url.searchParams.get("worker-zoom")).toBe("canonical\tpoll");
    expect(url.searchParams.get("offworker-zoom")).toBe("off\twait");
    expect(url.searchParams.get("inspect")).toBe(
      "tokio::runtime::task::harness::poll_future",
    );
  });

  it("rejects stale trace params when the current source is unshareable", () => {
    const href = buildRegionFlamegraphPopoutUrl(
      "?trace=old-url-source.bin",
      { startNs: 100, endNs: 900 },
      {
        regionWorkerZoom: ["current", "local"],
        regionOffworkerZoom: [],
        regionInspectFocus: null,
      },
      false,
    );

    expect(href).toBeNull();
  });
});
