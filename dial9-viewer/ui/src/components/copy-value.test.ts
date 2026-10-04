import { afterEach, describe, expect, it, vi } from "vitest";
import { copyText } from "../lib/url/copy-link.js";
import { copyValue } from "./copy-value.js";

vi.mock("../lib/url/copy-link.js", () => ({ copyText: vi.fn() }));

afterEach(() => {
  vi.useRealTimers();
  vi.restoreAllMocks();
});

const button = () => ({ textContent: "⎘", title: "Copy value", disabled: false });

describe("copy value feedback", () => {
  it("copies the full value and reports success only after the write completes", async () => {
    vi.useFakeTimers();
    let finish!: () => void;
    vi.mocked(copyText).mockImplementation(() => new Promise((resolve) => { finish = resolve; }));
    const btn = button();
    const value = 'long value\nwith "quotes" & <markup>';
    const pending = copyValue({ currentTarget: btn } as unknown as MouseEvent, value);
    expect(copyText).toHaveBeenCalledWith(value);
    expect(btn.textContent).toBe("⎘");
    expect(btn.disabled).toBe(true);
    finish();
    await pending;
    expect(btn.textContent).toBe("✓");
    expect(btn.title).toBe("Copied");
    expect(btn.disabled).toBe(false);
    await vi.advanceTimersByTimeAsync(800);
    expect(btn.textContent).toBe("⎘");
    expect(btn.title).toBe("Copy value");
  });

  it("reports a failed write instead of flashing success, and allows retry", async () => {
    vi.useFakeTimers();
    vi.mocked(copyText).mockRejectedValueOnce(new Error("Permission denied")).mockResolvedValue(undefined);
    const warn = vi.spyOn(console, "warn").mockImplementation(() => {});
    const btn = button();
    const event = { currentTarget: btn } as unknown as MouseEvent;
    await copyValue(event, "");
    expect(btn.textContent).toBe("!");
    expect(btn.title).toBe("Copy failed");
    expect(warn).toHaveBeenCalled();
    await vi.advanceTimersByTimeAsync(400);
    await copyValue(event, "");
    await vi.advanceTimersByTimeAsync(400);
    expect(btn.textContent).toBe("✓");
    await vi.advanceTimersByTimeAsync(400);
    expect(btn.textContent).toBe("⎘");
  });
});
