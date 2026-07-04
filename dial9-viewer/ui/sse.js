"use strict";

// Minimal Server-Sent Events client over `fetch`.
//
// We can't use the native `EventSource`: the aggregate endpoints authenticate
// bring-your-own-credentials via `x-dial9-aws-*` request headers (see creds.js),
// and `EventSource` cannot set request headers. So we read the
// `text/event-stream` body off a `fetch` response reader ourselves — the same
// pattern trace_parser.js uses for streaming gzip decode.
//
// This module exposes two pieces:
//   - `SseDecoder`, a pure incremental frame parser (feed it text chunks, get
//     back complete events). Pure + exported so it's unit-testable without a
//     network or DOM.
//   - `openSse(url, opts)`, which wires a `fetch` reader through the decoder and
//     invokes callbacks per event / on close / on error.
//
// We only parse the subset of the SSE grammar the server emits: `data:` lines
// (one JSON object per event) terminated by a blank line. `event:`/`id:`/`retry:`
// and comment (`:`) lines are tolerated and ignored.

// Incremental decoder. Feed it decoded text via `push(chunk)`; it returns an
// array of complete event payload strings (the concatenated `data:` lines of
// each frame) found so far. Call `flush()` at EOF to emit any trailing frame
// that wasn't terminated by a blank line.
class SseDecoder {
  constructor() {
    this._buf = "";
  }

  // Push a text chunk; returns an array of completed event data strings.
  push(chunk) {
    this._buf += chunk;
    const events = [];
    let idx;
    // Frames are separated by a blank line. Normalize CRLF to LF first so a
    // server (or proxy) using \r\n doesn't leave stray \r in the payload.
    this._buf = this._buf.replace(/\r\n/g, "\n");
    while ((idx = this._buf.indexOf("\n\n")) !== -1) {
      const frame = this._buf.slice(0, idx);
      this._buf = this._buf.slice(idx + 2);
      const data = parseFrame(frame);
      if (data != null) events.push(data);
    }
    return events;
  }

  // Emit a trailing, non-blank-line-terminated frame at EOF (if any).
  flush() {
    const rest = this._buf.trim();
    this._buf = "";
    if (!rest) return [];
    const data = parseFrame(rest);
    return data != null ? [data] : [];
  }
}

// Parse a single SSE frame (the text between blank lines) into its `data`
// payload — the `data:` lines joined by "\n" per the spec. Returns null when a
// frame carries no data (e.g. a keep-alive comment `: ...`).
function parseFrame(frame) {
  const dataLines = [];
  for (const rawLine of frame.split("\n")) {
    const line = rawLine;
    if (line === "" || line.startsWith(":")) continue; // comment / blank
    const colon = line.indexOf(":");
    const field = colon === -1 ? line : line.slice(0, colon);
    if (field !== "data") continue; // ignore event:/id:/retry:
    // Per spec, a single leading space after the colon is stripped.
    let value = colon === -1 ? "" : line.slice(colon + 1);
    if (value.startsWith(" ")) value = value.slice(1);
    dataLines.push(value);
  }
  if (dataLines.length === 0) return null;
  return dataLines.join("\n");
}

// Open an SSE stream over `fetch` and drive callbacks.
//
//   openSse(url, {
//     headers,            // request headers (e.g. Dial9Creds.headers())
//     signal,             // AbortSignal to cancel the stream
//     onEvent(obj),       // called with the parsed JSON per event
//     onError(err),       // called on network/HTTP error (not on abort)
//     onClose(),          // called when the server closes the stream cleanly
//   })
//
// Returns a promise that resolves when the stream ends for any reason — clean
// close (after onClose), abort via `signal` (quietly: neither onClose nor
// onError fires), or error (after onError). It never rejects.
async function openSse(url, opts = {}) {
  const { headers, signal, onEvent, onError, onClose } = opts;
  let resp;
  try {
    resp = await fetch(url, { headers, signal });
  } catch (err) {
    if (signal && signal.aborted) return; // caller cancelled — not an error
    if (onError) onError(err);
    return;
  }
  if (!resp.ok) {
    const body = await resp.text().catch(() => "");
    if (onError) onError(new Error(`HTTP ${resp.status}${body ? ": " + body : ""}`));
    return;
  }
  if (!resp.body || !resp.body.getReader) {
    if (onError) onError(new Error("response has no readable body for SSE"));
    return;
  }

  const reader = resp.body.getReader();
  const decoder = new TextDecoder("utf-8");
  const sse = new SseDecoder();

  const emit = (payload) => {
    if (!onEvent) return;
    let obj;
    try {
      obj = JSON.parse(payload);
    } catch (err) {
      // Shouldn't happen (the server emits one JSON object per event), but a
      // silently-dropped frame is hard to debug — log it. Rate is naturally
      // bounded (one per malformed frame).
      if (typeof console !== "undefined") {
        console.warn("Dial9Sse: dropping un-parseable event payload", err);
      }
      return;
    }
    onEvent(obj);
  };

  try {
    for (;;) {
      const { value, done } = await reader.read();
      if (done) break;
      const text = decoder.decode(value, { stream: true });
      if (!text) continue;
      for (const payload of sse.push(text)) emit(payload);
    }
    for (const payload of sse.flush()) emit(payload);
    if (onClose) onClose();
  } catch (err) {
    if (signal && signal.aborted) return; // cancelled mid-stream — not an error
    if (onError) onError(err);
  }
}

if (typeof window !== "undefined") {
  window.Dial9Sse = { SseDecoder, openSse };
}
if (typeof module !== "undefined" && module.exports) {
  module.exports = { SseDecoder, parseFrame, openSse };
}
