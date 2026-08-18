import { createRequire } from "node:module";
import { describe, expect, it } from "vitest";

const require = createRequire(import.meta.url);
const { SseDecoder, parseFrame } = require("../../sse.js") as {
  SseDecoder: new () => {
    push: (chunk: string) => string[];
    flush: () => string[];
  };
  parseFrame: (frame: string) => string | null;
};

describe("parseFrame", () => {
  it("collects data lines and ignores other SSE fields", () => {
    expect(parseFrame('data: {"a":1}')).toBe('{"a":1}');
    expect(parseFrame('data:{"a":1}')).toBe('{"a":1}');
    expect(parseFrame("data: a\ndata: b")).toBe("a\nb");
    expect(parseFrame(": keep-alive")).toBeNull();
    expect(parseFrame("event: ping\nid: 5")).toBeNull();
    expect(parseFrame("event: msg\ndata: hello")).toBe("hello");
  });
});

describe("SseDecoder", () => {
  it("emits complete frames and skips keep-alives", () => {
    const decoder = new SseDecoder();
    expect(
      decoder.push("data: 1\n\n: keep-alive\n\ndata: 2\n\ndata: 3\n\n"),
    ).toEqual(["1", "2", "3"]);
    expect(decoder.push("")).toEqual([]);
  });

  it("buffers frames split across chunks and terminators", () => {
    const decoder = new SseDecoder();
    expect(decoder.push('data: {"big":')).toEqual([]);
    expect(decoder.push(" true}\n")).toEqual([]);
    expect(decoder.push('\ndata: {"next":1}\n\n')).toEqual([
      '{"big": true}',
      '{"next":1}',
    ]);
  });

  it("normalizes CRLF frame boundaries", () => {
    const decoder = new SseDecoder();
    expect(decoder.push("data: a\r\n\r\n")).toEqual(["a"]);
  });

  it("flushes one trailing frame and is then idempotent", () => {
    const decoder = new SseDecoder();
    expect(decoder.push("data: last")).toEqual([]);
    expect(decoder.flush()).toEqual(["last"]);
    expect(decoder.flush()).toEqual([]);
  });

  it("preserves two JSON events when a chunk splits the second payload", () => {
    const decoder = new SseDecoder();
    const output = decoder.push(
      'data: {"files_folded":1}\n\ndata: {"files_fol',
    );
    output.push(...decoder.push('ded":2}\n\n'));
    expect(output).toEqual([
      '{"files_folded":1}',
      '{"files_folded":2}',
    ]);
  });
});
