// trace_parser.js - Binary trace parser using dial9-trace-format decoder
// Can be used in browser or Node.js

(function (exports) {
  "use strict";

  const MAX_EVENTS = Infinity; // no cap — use time range filtering for large traces

  function getTraceDecoder() {
    if (typeof require !== "undefined") {
      const path = require("path");
      return require(path.resolve(__dirname, "decode.js")).TraceDecoder;
    }
    // Browser: decode.js must be loaded before this script
    if (typeof TraceDecoder !== "undefined") return TraceDecoder;
    throw new Error(
      "TraceDecoder not found. Load decode.js before trace_parser.js"
    );
  }

  /** Parse a string/bigint/number to a JS number */
  function num(v) {
    if (typeof v === "number") return v;
    if (typeof v === "bigint") return Number(v);
    if (typeof v === "string" && v !== "")
      if (!isNaN(Number(v))) return Number(v);

    throw new Error(`Invalid number: ${v}`);
  }

  /** Decompress gzip data if detected, otherwise return as-is. */
  async function maybeGunzip(buf) {
    const b = buf instanceof ArrayBuffer ? new Uint8Array(buf) : buf;
    if (b.length < 2 || b[0] !== 0x1f || b[1] !== 0x8b) {
      return buf;
    }
    if (typeof DecompressionStream !== "undefined") {
      return await new Response(
        new Blob([b]).stream().pipeThrough(new DecompressionStream("gzip"))
      ).arrayBuffer();
    }
    // Fallback for older Node.js without DecompressionStream
    const zlib = require("zlib");
    const decompressed = zlib.gunzipSync(Buffer.from(b));
    return decompressed.buffer.slice(
      decompressed.byteOffset,
      decompressed.byteOffset + decompressed.byteLength
    );
  }

  /**
   * Whether `url` resolves to the same origin as the current page.
   *
   * Security-critical: credential headers (see `fetchTraces`) must never be
   * attached to a cross-origin request, or a crafted `?trace=https://attacker/`
   * link would exfiltrate the user's AWS credentials. Off-browser (Node tests),
   * there is no origin concept, so we treat everything as same-origin — the
   * exfiltration risk only exists in the browser.
   *
   * Conservative on failure: an unparseable URL is treated as cross-origin so
   * headers are withheld rather than sent.
   */
  function isSameOrigin(url) {
    if (typeof location === "undefined" || !location.origin) return true;
    try {
      return new URL(url, location.href).origin === location.origin;
    } catch {
      return false;
    }
  }

  /**
   * Fetch one or more trace URLs, gunzip each component individually, and
   * concatenate them into a single ArrayBuffer.
   *
   * The `trace` query parameter is repeatable: each component is fetched
   * independently (in parallel) and may be gzipped on its own — the S3 browser
   * points each at `/api/object`, which serves one file's raw, still-gzipped
   * bytes. We therefore ungzip every component here, then concatenate the raw
   * bytes. The trace decoder treats a concatenated stream as multiple segments
   * — a mid-stream `TRC\0` header resets the frame parser — so the combined
   * buffer parses as one trace.
   *
   * @param {string|string[]} urls one URL or a list of URLs (order preserved)
   * @param {{signal?: AbortSignal, headers?: Object}} [opts] `headers` is
   *   attached to every request (e.g. bring-your-own-credentials headers). This
   *   module stays storage-agnostic — the caller supplies the headers.
   * @returns {Promise<ArrayBuffer>}
   */
  async function fetchTraces(urls, opts = {}) {
    const list = Array.isArray(urls) ? urls : [urls];
    const parts = await Promise.all(
      list.map(async (url) => {
        // Only attach credential headers to same-origin requests. A
        // cross-origin `trace=` URL (e.g. a presigned S3 link, or an
        // attacker-crafted one) is fetched WITHOUT them, so the user's AWS
        // credentials can never be sent to a foreign host.
        const headers = isSameOrigin(url) ? opts.headers : undefined;
        const resp = await fetch(url, {
          signal: opts.signal,
          headers,
        });
        if (!resp.ok) throw new Error(`HTTP ${resp.status} fetching ${url}`);
        const raw = await maybeGunzip(await resp.arrayBuffer());
        return raw instanceof ArrayBuffer ? new Uint8Array(raw) : raw;
      })
    );
    if (parts.length === 1) return parts[0].buffer;
    let total = 0;
    for (const p of parts) total += p.length;
    const out = new Uint8Array(total);
    let off = 0;
    for (const p of parts) {
      out.set(p, off);
      off += p.length;
    }
    return out.buffer;
  }

  /**
   * Whether the current runtime can stream a `fetch` response body and gunzip
   * it incrementally. Node test runtimes (and older browsers) lack a real
   * streaming `response.body` or `DecompressionStream`; callers fall back to
   * the buffered `fetchTraces` + `parseTraceBuffer` path there.
   */
  function canStreamDecode() {
    return typeof ReadableStream !== "undefined" &&
      typeof DecompressionStream !== "undefined";
  }

  /**
   * Wrap an already-buffered `Uint8Array` in the minimal subset of the
   * `ReadableStreamDefaultReader` interface that {@link fetchTraceStream} uses
   * (`read()` / `cancel()`). Yields the whole buffer in one chunk, then EOF.
   * Used only as a fallback when a `fetch` response has no streamable `body`.
   */
  function oneShotReader(bytes) {
    let done = false;
    return {
      async read() {
        if (done) return { value: undefined, done: true };
        done = true;
        return { value: bytes, done: false };
      },
      async cancel() { done = true; },
    };
  }

  /**
   * Fetch a single trace URL and return an async iterable of raw (gunzipped, if
   * the body was gzipped) `Uint8Array` chunks. Pair with
   * {@link parseTraceStream} so download and decode overlap.
   *
   * We peek the first chunk's first two bytes for the gzip magic (1f 8b). If
   * present, the remaining body is piped through `DecompressionStream("gzip")`
   * after re-feeding the peeked chunk; otherwise chunks pass through raw.
   * `headers` follow the same same-origin credential-withholding rule as
   * {@link fetchTraces}.
   *
   * @param {string} url single trace URL
   * @param {{signal?: AbortSignal, headers?: Object}} [opts]
   * @returns {Promise<AsyncIterable<Uint8Array>>}
   */
  async function fetchTraceStream(url, opts = {}) {
    const headers = isSameOrigin(url) ? opts.headers : undefined;
    const resp = await fetch(url, { signal: opts.signal, headers });
    if (!resp.ok) throw new Error(`HTTP ${resp.status} fetching ${url}`);

    // Some runtimes hand back an ok response with no readable `body` stream
    // even though canStreamDecode() reported support (e.g. certain cached or
    // synthesized responses). Rather than throwing a bare TypeError on
    // `null.getReader()`, buffer the whole body and adapt it to the same
    // reader interface so the gzip-sniff + DecompressionStream path below is
    // identical — we just lose the download/parse overlap for this response.
    const reader = resp.body
      ? resp.body.getReader()
      : oneShotReader(new Uint8Array(await resp.arrayBuffer()));
    // Read the first non-empty chunk to sniff the gzip magic.
    let first = null;
    for (;;) {
      const { value, done } = await reader.read();
      if (done) break;
      if (value && value.byteLength > 0) {
        first = value instanceof Uint8Array ? value : new Uint8Array(value);
        break;
      }
    }

    const isGzip = first != null && first.length >= 2 &&
      first[0] === 0x1f && first[1] === 0x8b;

    // A ReadableStream that re-emits the peeked first chunk then drains the
    // rest of the body reader.
    const rawStream = new ReadableStream({
      start(controller) {
        if (first != null) controller.enqueue(first);
      },
      async pull(controller) {
        const { value, done } = await reader.read();
        if (done) { controller.close(); return; }
        if (value && value.byteLength > 0) controller.enqueue(value);
      },
      cancel(reason) { return reader.cancel(reason); },
    });

    const byteStream = isGzip
      ? rawStream.pipeThrough(new DecompressionStream("gzip"))
      : rawStream;

    return {
      [Symbol.asyncIterator]() {
        const r = byteStream.getReader();
        return {
          async next() {
            const { value, done } = await r.read();
            if (done) return { done: true, value: undefined };
            const u8 = value instanceof Uint8Array ? value : new Uint8Array(value);
            return { done: false, value: u8 };
          },
          async return() { await r.cancel(); return { done: true, value: undefined }; },
        };
      },
    };
  }

  /**
   * @typedef {{
   *   eventType: number,
   *   timestamp: number,
   *   workerId: number,
   *   localQueue: number,
   *   globalQueue: number,
   *   cpuTime: number,
   *   schedWait: number,
   *   taskId: number,
   *   spawnLocId: string|null,
   *   spawnLoc: string|null,
   *   wakerTaskId?: number,
   *   wokenTaskId?: number,
   *   targetWorker?: number,
   * }} TraceEvent
   */

  /**
   * @typedef {{
   *   timestamp: number,
   *   workerId: number,
   *   tid: number,
   *   source: number,
   *   callchain: string[],
   *   cpu: number|null,
   * }} CpuSample
   */

  /**
   * @typedef {{ symbol: string, location: string|null }} SymbolFrame
   */

  /**
   * @typedef {{
   *   magic: "D9TF",
   *   version: number,
   *   events: TraceEvent[],
   *   minTs: number|null,
   *   maxTs: number|null,
   *   recordMinTs: number|null,
   *   recordMaxTs: number|null,
   *   truncated: boolean,
   *   hasCpuTime: boolean,
   *   hasSchedWait: boolean,
   *   hasTaskTracking: boolean,
   *   spawnLocations: Map<string, string>,
   *   taskSpawnLocs: Map<number, string|null>,
   *   taskSpawnTimes: Map<number, number>,
   *   taskTerminateTimes: Map<number, number>,
   *   taskInstrumented: Map<number, boolean>,
   *   cpuSamples: CpuSample[],
   *   callframeSymbols: Map<string, SymbolFrame|SymbolFrame[]>,
   *   threadNames: Map<number, string>,
   *   runtimeWorkers: Map<string, number[]>,
   * }} ParsedTrace
   */

  const EVENT_TYPES = {
    PollStart: 0,
    PollEnd: 1,
    WorkerPark: 2,
    WorkerUnpark: 3,
    QueueSample: 4,
    WakeEvent: 9,
  };

  /**
   * Sentinel `workerId` used for CPU samples that cannot be confidently
   * attributed to a specific worker. Matches the producer-side
   * `WorkerId::UNKNOWN` value (also `WorkerId::BLOCKING - 1`).
   */
  const OFF_WORKER_WORKER_ID = 255;

  /**
   * Derive block-in-place gaps from WorkerPark/WorkerUnpark events and
   * rewrite `cpuSamples[i].workerId` for samples that fall inside a gap.
   *
   * See `CONTEXT.md` (Block-in-place gap), ADR-0001 and ADR-0002. The
   * detection algorithm is: for each worker `W`, track the currently-bound
   * tid via park/unpark events. When the next park/unpark on `W` carries a
   * tid that doesn't match the currently-bound tid, a `block_in_place`
   * handoff happened at an unknown instant in the interval. The whole
   * interval is a "gap"; samples on the old or new tid in this interval
   * cannot be confidently attributed to `W` and have their `workerId`
   * rewritten to {@link OFF_WORKER_WORKER_ID}.
   *
   * Old traces lacking `tid` on park/unpark events are silently ignored
   * — gap detection is a no-op on them, no rewriting happens.
   *
   * Mutates `cpuSamples` in place. Returns the gap list sorted by start.
   *
   * @param {Array<TraceEvent>} events events sorted by timestamp
   * @param {Array<CpuSample>} cpuSamples cpu samples to (possibly) rewrite
   * @returns {Array<{workerId:number, fromTid:number, toTid:number, startNs:number, endNs:number}>}
   */
  function deriveBlockInPlaceGaps(events, cpuSamples) {
    const gaps = [];
    // Per-worker state: { currentTid: number|null, lastEventTs: number }.
    // null currentTid means the worker is parked (or has never unparked).
    const state = new Map();

    for (const e of events) {
      if (e.eventType !== EVENT_TYPES.WorkerPark &&
          e.eventType !== EVENT_TYPES.WorkerUnpark) continue;
      // Ignore events without a tid (older traces predate the field).
      if (e.tid === undefined) continue;

      const w = e.workerId;
      const s = state.get(w);
      if (s === undefined) {
        // First event for this worker. Establish the binding.
        state.set(w, {
          currentTid: e.eventType === EVENT_TYPES.WorkerUnpark ? e.tid : null,
          lastEventTs: e.timestamp,
          // For a Park-first worker, we still know `tid` was bound to W up
          // until the park, but we can't know for how long. Track the tid
          // so a subsequent (mismatched) event flags a gap.
          lastSeenTid: e.tid,
        });
        continue;
      }

      // Check for handoff: the event's tid differs from the tid we believe
      // is currently bound to this worker.
      const expectedTid = s.currentTid != null ? s.currentTid : s.lastSeenTid;
      if (expectedTid !== e.tid) {
        gaps.push({
          workerId: w,
          fromTid: expectedTid,
          toTid: e.tid,
          startNs: s.lastEventTs,
          endNs: e.timestamp,
        });
      }

      // Update state regardless of whether a gap was detected.
      s.currentTid = e.eventType === EVENT_TYPES.WorkerUnpark ? e.tid : null;
      s.lastEventTs = e.timestamp;
      s.lastSeenTid = e.tid;
    }

    // Sort gaps by start timestamp for downstream consumers.
    gaps.sort((a, b) => a.startNs - b.startNs);

    if (gaps.length === 0) return gaps;

    // Build per-worker gap lists for sample rewriting.
    // We index by worker because gaps belong to a worker, but we suppress
    // samples by tid: any sample whose tid matches the gap's fromTid OR
    // toTid, falling inside the gap window, is unattributable.
    const gapsByWorker = new Map();
    for (const g of gaps) {
      let arr = gapsByWorker.get(g.workerId);
      if (!arr) { arr = []; gapsByWorker.set(g.workerId, arr); }
      arr.push(g);
    }

    // Rewrite cpu samples in-place. For each sample, check the wire
    // workerId's gap list (if any) and any gap matching the sample's tid.
    // We iterate gaps directly per sample because:
    //  - the per-worker gap count is small (typically 0–few per trace);
    //  - samples might land inside a gap whose worker_id doesn't match
    //    the wire value (the wire value is unreliable, ADR-0001), so we
    //    must check by tid against ALL gaps the tid is involved in.
    // To avoid an O(samples * gaps) blowup, build per-tid gap lists too.
    const gapsByTid = new Map();
    for (const g of gaps) {
      for (const t of [g.fromTid, g.toTid]) {
        let arr = gapsByTid.get(t);
        if (!arr) { arr = []; gapsByTid.set(t, arr); }
        arr.push(g);
      }
    }
    // Sort each tid's list by start so we can early-exit.
    for (const arr of gapsByTid.values()) {
      arr.sort((a, b) => a.startNs - b.startNs);
    }

    for (const sample of cpuSamples) {
      const tidGaps = gapsByTid.get(sample.tid);
      if (!tidGaps) continue;
      const ts = sample.timestamp;
      for (const g of tidGaps) {
        if (g.startNs > ts) break; // sorted by start, no more matches
        if (ts < g.endNs) {
          // sample falls within [startNs, endNs)
          sample.workerId = OFF_WORKER_WORKER_ID;
          break;
        }
      }
    }

    return gaps;
  }

  /**
   * Parse dial9 trace data from a buffer, file path, or directory.
   *
   * - Buffer/ArrayBuffer/Uint8Array: returns Promise<ParsedTrace> (browser compatible).
   * - String (file path): returns AsyncIterable yielding one ParsedTrace (Node.js only).
   * - String (directory): returns AsyncIterable yielding one ParsedTrace per file,
   *   with parallel parsing and caching (Node.js only).
   *
   * In the browser, fetch trace data via the viewer API and pass the ArrayBuffer.
   *
   * @param {ArrayBuffer|Uint8Array|string} input - Binary data, file path, or directory path
   * @param {Object} [options] - Optional parsing options
   * @param {number} [options.maxEvents] - Maximum number of events to parse (default: Infinity)
   * @param {number} [options.startTime] - Start of time range filter (absolute ns, inclusive)
   * @param {number} [options.endTime] - End of time range filter (absolute ns, inclusive)
   * @param {function} [options.onParseProgress] - Called with {done, total, file} as files complete
   * @param {boolean} [options.cache] - Enable disk caching for directories (default: true)
   * @param {boolean} [options.parallel] - Enable parallel parsing for directories (default: true)
   * @param {boolean} [options.force] - Ignore cached results and re-parse (default: false)
   * @param {number} [options.sample] - Only parse N evenly-spaced files from a directory
   * @returns {AsyncIterable<ParsedTrace>}
   */
  function parseTrace(input, options) {
    if (typeof input === 'string') {
      if (typeof require === 'undefined') {
        throw new Error(
          'File/directory paths require Node.js. In the browser, fetch trace ' +
          'data via the viewer API (e.g. /api/trace) and pass the ArrayBuffer ' +
          'to parseTrace().'
        );
      }
      const fs = require('fs');
      const stat = fs.statSync(input);
      if (stat.isDirectory()) {
        return parseTraceDir(input, options);
      }
      // Single file path: async iterable yielding one trace
      return wrapSingle(parseTraceBuffer(fs.readFileSync(input), options));
    }
    // Buffer: return Promise<ParsedTrace> directly (backwards compatible with browser)
    return parseTraceBuffer(input, options);
  }

  /** Wrap a Promise<ParsedTrace> as an async iterable that yields once.
   *  Also thenable, so `await parseTrace('file.bin')` works directly. */
  function wrapSingle(promise) {
    const iterable = {
      [Symbol.asyncIterator]() {
        let done = false;
        return { async next() {
          if (done) return { done: true, value: undefined };
          done = true;
          return { done: false, value: await promise };
        }};
      },
      then(resolve, reject) { return promise.then(resolve, reject); },
      catch(reject) { return promise.catch(reject); },
      finally(cb) { return promise.finally(cb); },
    };
    return iterable;
  }

  const UNCAPPED_FRAMES = new Set([
    "TaskSpawnEvent",
    "TaskTerminateEvent",
    "CpuSampleEvent",
    "TaskDumpEvent",
    "SymbolTableEntry",
    "SegmentMetadataEvent",
    "ClockSyncEvent",
  ]);
  const TRACE_BOUND_EXCLUDED_FRAMES = new Set([
    "SymbolTableEntry",
    "SegmentMetadataEvent",
    "ClockSyncEvent",
  ]);
  // Legacy classifier: epoch ns are ~1e18, monotonic ns are much smaller.
  // 2020 is a practical floor that separates those ranges.
  const LEGACY_EPOCH_FLOOR_MS = 1_577_836_800_000; // 2020-01-01

  /**
   * @private Build the mutable accumulator that {@link processFrame} fills and
   * {@link finalizeParse} drains. Shared by the whole-buffer
   * ({@link parseTraceBuffer}) and streaming ({@link parseTraceStream}) paths
   * so there is exactly one copy of the frame-handling and post-processing
   * logic. Returns the same `ParsedTrace` for the same concatenated bytes
   * regardless of how those bytes were chunked.
   */
  function createParseState(options) {
    const maxEvents = (options && options.maxEvents != null) ? options.maxEvents : MAX_EVENTS;
    const startTime = (options && options.startTime != null) ? options.startTime : 0;
    const endTime = (options && options.endTime != null) ? options.endTime : Infinity;
    const hasTimeFilter = startTime > 0 || endTime < Infinity;
    return {
      maxEvents, startTime, endTime, hasTimeFilter,
      events: [],
      spawnLocations: new Map(),
      taskSpawnLocs: new Map(),
      taskSpawnTimes: new Map(),
      taskTerminateTimes: new Map(),
      taskInstrumented: new Map(), // taskId -> bool (true if spawned via TelemetryHandle::spawn)
      callframeSymbols: new Map(),
      cpuSamples: [],
      allocEvents: [],
      freeEvents: [],
      memoryOverflows: [],
      threadNames: new Map(),
      tidToWorker: new Map(), // tid → workerId (stable mapping from park/unpark events)
      runtimeWorkers: new Map(), // runtime name → [workerId, ...]
      taskDumps: new Map(), // taskId → [{timestamp, callchain}] sorted by timestamp
      customEvents: [], // unrecognized event types: {name, timestamp, fields}
      // { monotonicNs, realtimeNs } anchors used to recover wall clock.
      clockSyncAnchors: [],
      legacySegmentMetaWallNs: null,
      // Smallest monotonic ts seen across all event frames.
      // Used as the monotonic timestamp for the legacy synthesized anchor.
      minMonoTs: null,
      recordMinTs: Infinity,
      recordMaxTs: -Infinity,
    };
  }

  /**
   * @private Handle a single decoded frame, mutating `state`. `dec` is the
   * decoder the frame came from (used only to read a custom event's schema
   * units). This is the single shared switch body — do not duplicate it.
   */
  function processFrame(frame, state, dec) {
    const { startTime, endTime } = state;
    if (frame.type !== "event") return;
    const v = frame.values;
    const ts = num(frame.timestamp_ns);
    // Track smallest monotonic ts for legacy anchor synthesis.
    // Skip SegmentMetadata (legacy wall clock) and SymbolTableEntry.
    if (
      ts != null &&
      frame.name !== "SegmentMetadataEvent" &&
      frame.name !== "SymbolTableEntry" &&
      (state.minMonoTs == null || ts < state.minMonoTs)
    ) {
      state.minMonoTs = ts;
    }

    const capped = state.events.length >= state.maxEvents;
    if (capped && !UNCAPPED_FRAMES.has(frame.name)) return;

    // Time range filtering: skip events outside the requested range
    // (uncapped frames like symbols/metadata are always processed)
    const inTimeRange = ts >= startTime && ts <= endTime;
    if (!inTimeRange && !UNCAPPED_FRAMES.has(frame.name)) return;
    if (inTimeRange && !TRACE_BOUND_EXCLUDED_FRAMES.has(frame.name)) {
      if (ts != null) {
        if (ts < state.recordMinTs) state.recordMinTs = ts;
        if (ts > state.recordMaxTs) state.recordMaxTs = ts;
      }
    }

    const events = state.events;
    const spawnLocations = state.spawnLocations;
    const taskSpawnLocs = state.taskSpawnLocs;
    const taskSpawnTimes = state.taskSpawnTimes;
    const taskTerminateTimes = state.taskTerminateTimes;
    const taskInstrumented = state.taskInstrumented;
    const callframeSymbols = state.callframeSymbols;
    const cpuSamples = state.cpuSamples;
    const allocEvents = state.allocEvents;
    const freeEvents = state.freeEvents;
    const memoryOverflows = state.memoryOverflows;
    const threadNames = state.threadNames;
    const tidToWorker = state.tidToWorker;
    const runtimeWorkers = state.runtimeWorkers;
    const taskDumps = state.taskDumps;
    const customEvents = state.customEvents;
    const clockSyncAnchors = state.clockSyncAnchors;

    switch (frame.name) {
        case "PollStartEvent": {
          const spawnLoc = v.spawn_loc || null;
          if (spawnLoc) spawnLocations.set(spawnLoc, spawnLoc);
          const taskId = num(v.task_id);
          if (taskId && spawnLoc && !taskSpawnLocs.has(taskId)) {
            taskSpawnLocs.set(taskId, spawnLoc);
          }
          events.push({
            eventType: 0,
            timestamp: ts,
            workerId: num(v.worker_id),
            localQueue: num(v.local_queue),
            globalQueue: 0,
            cpuTime: 0,
            schedWait: 0,
            taskId,
            spawnLocId: spawnLoc,
            spawnLoc,
          });
          break;
        }
        case "PollEndEvent":
          events.push({
            eventType: 1,
            timestamp: ts,
            workerId: num(v.worker_id),
            globalQueue: 0,
            localQueue: 0,
            cpuTime: 0,
            schedWait: 0,
            taskId: 0,
            spawnLocId: null,
            spawnLoc: null,
          });
          break;
        case "WorkerParkEvent":
          if (v.tid != null) tidToWorker.set(num(v.tid), num(v.worker_id));
          events.push({
            eventType: 2,
            timestamp: ts,
            workerId: num(v.worker_id),
            localQueue: num(v.local_queue),
            cpuTime: num(v.cpu_time_ns),
            // tid was added later; old traces won't have it. Leave undefined
            // so the block-in-place gap detection can skip them.
            tid: v.tid != null ? num(v.tid) : undefined,
            globalQueue: 0,
            schedWait: 0,
            taskId: 0,
            spawnLocId: null,
            spawnLoc: null,
          });
          break;
        case "WorkerUnparkEvent":
          if (v.tid != null) tidToWorker.set(num(v.tid), num(v.worker_id));
          events.push({
            eventType: 3,
            timestamp: ts,
            workerId: num(v.worker_id),
            localQueue: num(v.local_queue),
            cpuTime: num(v.cpu_time_ns),
            schedWait: num(v.sched_wait_ns),
            // tid was added later; old traces won't have it. Leave undefined
            // so the block-in-place gap detection can skip them.
            tid: v.tid != null ? num(v.tid) : undefined,
            globalQueue: 0,
            taskId: 0,
            spawnLocId: null,
            spawnLoc: null,
          });
          break;
        case "QueueSampleEvent":
          events.push({
            eventType: 4,
            timestamp: ts,
            globalQueue: num(v.global_queue),
            workerId: 0,
            localQueue: 0,
            cpuTime: 0,
            schedWait: 0,
            taskId: 0,
            spawnLocId: null,
            spawnLoc: null,
          });
          break;
        case "TaskSpawnEvent": {
          const taskId = num(v.task_id);
          const spawnLoc = v.spawn_loc || null;
          const instrumented = v.instrumented ?? true;
          if (spawnLoc) spawnLocations.set(spawnLoc, spawnLoc);
          taskSpawnLocs.set(taskId, spawnLoc);
          taskSpawnTimes.set(taskId, ts);
          taskInstrumented.set(taskId, !!instrumented);
          break;
        }
        case "TaskTerminateEvent":
          taskTerminateTimes.set(num(v.task_id), ts);
          break;
        case "WakeEventEvent":
          events.push({
            eventType: 9,
            timestamp: ts,
            workerId: num(v.target_worker),
            wakerTaskId: num(v.waker_task_id),
            wokenTaskId: num(v.woken_task_id),
            targetWorker: num(v.target_worker),
            globalQueue: 0,
            localQueue: 0,
            cpuTime: 0,
            schedWait: 0,
            taskId: 0,
            spawnLocId: null,
            spawnLoc: null,
          });
          break;
        case "CpuSampleEvent": {
          const chain = (v.callchain || []).map(
            (addr) => "0x" + BigInt(addr).toString(16)
          );
          // `cpu` is encoded as OptionalVarint: null when the backend could
          // not determine the CPU. Varints decode as strings for BigInt safety;
          // CPU ids always fit in a Number.
          const cpu = v.cpu == null ? null : Number(v.cpu);
          cpuSamples.push({
            timestamp: ts,
            workerId: num(v.worker_id),
            tid: num(v.tid),
            source: num(v.source),
            callchain: chain,
            cpu,
          });
          const tn = v.thread_name;
          if (tn) {
            threadNames.set(num(v.tid), tn);
          }
          break;
        }
        case "TaskDumpEvent": {
          const taskId = num(v.task_id);
          const chain = (v.callchain || []).map(
            (addr) => "0x" + BigInt(addr).toString(16)
          );
          if (!taskDumps.has(taskId)) taskDumps.set(taskId, []);
          taskDumps.get(taskId).push({ timestamp: ts, callchain: chain });
          break;
        }
        case "AllocEvent": {
          const chain = (v.callchain || []).map(
            (addr) => "0x" + BigInt(addr).toString(16)
          );
          allocEvents.push({
            timestamp: ts,
            tid: num(v.tid),
            size: num(v.size),
            addr: BigInt(v.addr || 0).toString(),
            callchain: chain,
          });
          break;
        }
        case "FreeEvent": {
          freeEvents.push({
            timestamp: ts,
            tid: num(v.tid),
            addr: BigInt(v.addr || 0).toString(),
            size: num(v.size),
            allocTimestampNs: num(v.alloc_timestamp_ns),
          });
          break;
        }
        case "MemoryProfileOverflowEvent": {
          memoryOverflows.push({
            timestamp: ts,
            droppedAllocs: num(v.dropped_allocs),
            droppedFrees: num(v.dropped_frees),
          });
          break;
        }
        case "ClockSyncEvent": {
          const real = num(v.realtime_ns);
          if (real > 0) {
            clockSyncAnchors.push({ monotonicNs: ts, realtimeNs: real });
          }
          break;
        }
        case "SegmentMetadataEvent": {
          // If this looks epoch-scale, treat it as legacy wall clock.
          if (
            state.legacySegmentMetaWallNs == null &&
            ts != null &&
            ts / 1e6 >= LEGACY_EPOCH_FLOOR_MS
          ) {
            state.legacySegmentMetaWallNs = ts;
          }
          const entries = v.entries || {};
          for (const [key, val] of Object.entries(entries)) {
            if (key.startsWith("runtime.")) {
              const name = key.slice("runtime.".length);
              const ids = val
                .split(",")
                .map(Number)
                .filter((n) => !isNaN(n));
              if (ids.length > 0) runtimeWorkers.set(name, ids);
            }
          }
          break;
        }
        case "SymbolTableEntry": {
          const addrKey = "0x" + BigInt(v.addr).toString(16);
          const depth = Number(v.inline_depth || 0);
          const sf = v.source_file || "";
          const sl = Number(v.source_line || 0);
          const location = sf ? (sl ? `${sf}:${sl}` : sf) : null;
          const entry = { symbol: v.symbol_name, location };
          if (depth === 0) {
            // Outermost frame: store directly (or as first element of array)
            const existing = callframeSymbols.get(addrKey);
            if (Array.isArray(existing)) {
              existing[0] = entry;
            } else {
              callframeSymbols.set(addrKey, entry);
            }
          } else {
            // Inlined frame: promote to array
            let arr = callframeSymbols.get(addrKey);
            if (!Array.isArray(arr)) {
              arr = [arr || { symbol: addrKey, location: null }];
              callframeSymbols.set(addrKey, arr);
            }
            arr[depth] = entry;
          }
          break;
        }
        default: {
          // Unrecognized event type: capture as a custom event
          if (ts != null) {
            customEvents.push({
              name: frame.name,
              timestamp: ts,
              fields: v,
              units: dec.schemas.get(frame.typeId)?.units || null,
            });
          }
          break;
        }
    }
  }

  /**
   * @private Run the post-frame-loop passes (clock anchors, tid→worker
   * resolution, block-in-place gaps) and assemble the final `ParsedTrace`.
   * Identical for the whole-buffer and streaming paths. `version` is read from
   * the decoder after the last frame.
   */
  function finalizeParse(state, version) {
    const {
      events, spawnLocations, taskSpawnLocs, taskSpawnTimes, taskTerminateTimes,
      taskInstrumented, callframeSymbols, cpuSamples, allocEvents, freeEvents,
      memoryOverflows, threadNames, tidToWorker, runtimeWorkers, taskDumps,
      customEvents, clockSyncAnchors, maxEvents, startTime, endTime, hasTimeFilter,
    } = state;

    // Legacy fallback: synthesize an anchor from legacy SegmentMetadata wall
    // time + earliest monotonic event timestamp. This is best-effort only.
    if (
      clockSyncAnchors.length === 0 &&
      state.legacySegmentMetaWallNs != null &&
      state.minMonoTs != null
    ) {
      clockSyncAnchors.push({
        monotonicNs: state.minMonoTs,
        realtimeNs: state.legacySegmentMetaWallNs,
      });
    }

    clockSyncAnchors.sort((a, b) => {
      if (a.monotonicNs < b.monotonicNs) return -1;
      if (a.monotonicNs > b.monotonicNs) return 1;
      return 0;
    });

    // Sort task dumps by timestamp for efficient lookup during rendering
    for (const arr of taskDumps.values()) {
      arr.sort((a, b) => a.timestamp - b.timestamp);
    }

    let clockOffsetNs = null;
    if (clockSyncAnchors.length > 0) {
      const a0 = clockSyncAnchors[0];
      clockOffsetNs = a0.realtimeNs - a0.monotonicNs;
    }
    // Keep the historical event-only bounds for runtime analysis consumers.
    let evMinTs = Infinity, evMaxTs = -Infinity;
    for (let i = 0; i < events.length; i++) {
      const t = events[i].timestamp;
      if (t < evMinTs) evMinTs = t;
      if (t > evMaxTs) evMaxTs = t;
    }

    // Second pass: derive worker attribution from WorkerPark/WorkerUnpark
    // tid fields, detect block-in-place gaps, and rewrite cpuSamples.workerId.
    // See ADR-0001 (worker_id derived at analysis) and ADR-0002 (block-in-place
    // gap is unknowable).
    //
    // Samples are attributed by tid: resolve each through the tid -> worker map.
    // Leave the wire value untouched when the tid is unmapped, so legacy traces
    // (park/unpark without a tid) keep their pre-resolved worker id.
    for (const sample of cpuSamples) {
      const w = tidToWorker.get(sample.tid);
      if (w !== undefined) sample.workerId = w;
    }
    const blockInPlaceGaps = deriveBlockInPlaceGaps(events, cpuSamples);

    return {
      magic: "D9TF",
      version,
      events,
      minTs: events.length > 0 ? evMinTs : null,
      maxTs: events.length > 0 ? evMaxTs : null,
      recordMinTs: state.recordMinTs < Infinity ? state.recordMinTs : null,
      recordMaxTs: state.recordMaxTs > -Infinity ? state.recordMaxTs : null,
      truncated: events.length >= maxEvents,
      timeFiltered: hasTimeFilter,
      filterStartTime: hasTimeFilter ? startTime : null,
      filterEndTime: hasTimeFilter ? endTime : null,
      hasCpuTime: true,
      hasSchedWait: true,
      hasTaskTracking: true,
      spawnLocations,
      taskSpawnLocs,
      taskSpawnTimes,
      taskInstrumented,
      cpuSamples,
      allocEvents,
      freeEvents,
      memoryOverflows,
      callframeSymbols,
      threadNames,
      tidToWorker,
      taskTerminateTimes,
      runtimeWorkers,
      customEvents,
      taskDumps,
      clockSyncAnchors,
      clockOffsetNs,
      blockInPlaceGaps,
    };
  }

  /** @private Parse a binary trace buffer. */
  async function parseTraceBuffer(buffer, options) {
    buffer = await maybeGunzip(buffer);
    const onProgress = (options && options.onParseProgress) || null;
    const YIELD_BYTES = 100 * 1024; // yield to browser every 100KB
    const TD = getTraceDecoder();
    const dec = new TD(
      buffer instanceof ArrayBuffer ? new Uint8Array(buffer) : buffer
    );
    if (!dec.decodeHeader()) throw new Error("Invalid trace header");
    const totalBytes = dec.byteLength;
    const state = createParseState(options);

    let lastYieldPos = 0;
    let frame;
    while ((frame = dec.nextFrame()) !== null) {
      // Yield to browser periodically so spinner can update
      if (onProgress && dec.position - lastYieldPos >= YIELD_BYTES) {
        lastYieldPos = dec.position;
        onProgress({ bytesRead: dec.position, totalBytes, eventCount: state.events.length });
        await new Promise((r) => setTimeout(r, 0));
      }
      processFrame(frame, state, dec);
    }

    return finalizeParse(state, dec.version);
  }

  /**
   * Parse a trace from an async stream of raw (already-gunzipped) byte chunks.
   * Decoding overlaps with the network download: each chunk is appended to a
   * growable buffer, and all *complete* frames it newly exposes are drained
   * immediately; an incomplete trailing frame is rolled back and re-attempted
   * once the next chunk arrives. The same {@link processFrame} /
   * {@link finalizeParse} logic runs as for {@link parseTraceBuffer}, so the
   * result is byte-for-byte identical to parsing the concatenated buffer.
   *
   * @param {AsyncIterable<Uint8Array>} chunks raw trace bytes (post-gunzip)
   * @param {Object} [options] same options as {@link parseTrace}, plus
   *   `onParseProgress({bytesRead, totalBytes, eventCount})`. `totalBytes` is
   *   `null` here — when streaming we don't know the trace's full size until the
   *   stream ends, so callers must not compute a percentage from it.
   * @returns {Promise<ParsedTrace>}
   */
  async function parseTraceStream(chunks, options) {
    const onProgress = (options && options.onParseProgress) || null;
    const TD = getTraceDecoder();
    const state = createParseState(options);

    // Coalesce incoming chunks before draining. The browser's
    // DecompressionStream("gzip") emits tiny ~16KB chunks (measured: ~708
    // chunks of median/max 16384 bytes for the 10.27MB demo trace). Draining
    // on every chunk pays a full-tail grow() copy + decode setup ~708 times,
    // which dominates parse time (~66-78% overhead vs the whole-buffer
    // baseline). Waiting until ≥256KB of undrained bytes have accumulated
    // collapses those ~708 grow+drain cycles into ~40 and brings streaming back
    // to/below the whole-buffer baseline. The 5-byte header is still decoded as
    // soon as it arrives so a tiny trace (well under 256KB) still parses.
    const MIN_DRAIN_BYTES = 256 * 1024;
    // Yield a macrotask (setTimeout(0)) to the browser on a wall-clock cadence
    // rather than per-chunk or per-byte. KEY FACT: the `for await` over `chunks`
    // already awaits each decompressed chunk, so the network read +
    // DecompressionStream pump make progress on their own — the macrotask yield
    // does NOT drive download/parse overlap. Its ONLY purpose is to hand the
    // main thread back to the browser so it can repaint the spinner. Each yield
    // therefore permits (roughly) one repaint, and a byte cadence fired ~100
    // times for a 10MB trace — ~100 paints during an ~8s parse. Throttling by
    // elapsed wall-clock time instead caps paints to a few per second
    // regardless of trace size, with zero effect on overlap or decode output.
    // We don't drop the yield entirely: the spinner must still tick a few
    // times/sec so the user sees progress. onProgress still fires on EVERY
    // batch drain (cheap) so the event/byte counters stay current; only the
    // (relatively expensive) repaint yield is rate-limited.
    const PAINT_INTERVAL_MS = 200; // at most ~5 repaint yields per second

    // Unconsumed-tail accumulator. The decoder reads pooled strings/stacks via
    // its `stringPool`/`stackPool` maps (resolved values are copied into the
    // maps), and event field reads only touch the current frame's bytes — never
    // earlier offsets. So once a frame is fully decoded its bytes are dead, and
    // we can drop the consumed prefix after each drain. That keeps `acc`
    // bounded by (largest single frame + one chunk) and makes appends O(tail)
    // instead of O(total) — avoiding O(n²) blow-up on many small chunks.
    let acc = new Uint8Array(0);
    let dec = null;
    let headerDecoded = false;
    let sawAnyBytes = false;
    let consumedBytes = 0; // total bytes already decoded (for progress only)

    function grow(chunk) {
      if (chunk.length === 0) return;
      // Always copy into a buffer sized EXACTLY to the accumulated bytes.
      // Aliasing an incoming chunk (which may be a `subarray` view into a
      // larger underlying ArrayBuffer) is unsafe: the field decoders build
      // `new Uint8Array(view.buffer, …, len)` directly on the underlying
      // buffer, bypassing the DataView's length bound, and would happily read
      // bytes that lie past the logical end of the stream-so-far. Keeping
      // `acc.buffer.byteLength === acc.length` makes such over-reads throw
      // RangeError (→ needMoreBytes), which is exactly the incomplete-frame
      // signal the streaming loop relies on.
      const next = new Uint8Array(acc.length + chunk.length);
      next.set(acc, 0);
      next.set(chunk, acc.length);
      acc = next;
    }

    // Drain every complete frame currently in `acc`, then drop the consumed
    // prefix so the accumulator holds only the (incomplete) trailing frame.
    function drainComplete() {
      for (;;) {
        const snap = dec.snapshot();
        const frame = dec.nextFrame();
        if (frame === null) {
          if (dec.needMoreBytes) {
            // Partial frame at the tail: undo any positional advance so the
            // re-wrapped decoder re-attempts this frame from the same offset.
            dec.restore(snap);
          }
          break;
        }
        processFrame(frame, state, dec);
      }
      // Drop everything the decoder has already consumed. `dec.position` points
      // at the start of the incomplete trailing frame (or end-of-buffer).
      const keepFrom = dec.position;
      if (keepFrom > 0) {
        consumedBytes += keepFrom;
        acc = acc.slice(keepFrom);
      }
    }

    // Decode the header (once ≥5 bytes are buffered) and then drain every
    // complete frame currently in `acc`. Shared by the batched in-loop path and
    // the final end-of-stream flush so both go through identical logic.
    function drainBatch() {
      if (!headerDecoded) {
        // Need at least the 5-byte header before any frames. Wait for more.
        if (acc.length < 5) return;
        dec = new TD(acc);
        dec.enableStreaming();
        if (!dec.decodeHeader()) throw new Error("Invalid trace header");
        headerDecoded = true;
      } else {
        // Re-wrap the decoder over the grown tail, rebasing its position to 0
        // (the consumed prefix was dropped by the previous drainComplete()).
        // Accumulated state (schemas, pools, timestamp base) is preserved.
        dec.setBuffer(acc);
        dec.rewindToStart();
      }
      drainComplete();
    }

    // Wall-clock source that works in browser and Node. Node 16+ exposes a
    // global `performance`, but guard anyway in case it's absent.
    const nowMs = (typeof performance !== "undefined" && performance.now)
      ? () => performance.now()
      : () => Date.now();
    let lastYieldMs = nowMs(); // wall-clock time of the last macrotask yield
    for await (const rawChunk of chunks) {
      const chunk = rawChunk instanceof Uint8Array
        ? rawChunk
        : new Uint8Array(rawChunk);
      if (chunk.length === 0) continue;
      sawAnyBytes = true;
      grow(chunk);

      // Batch tiny chunks: keep accumulating until the undrained tail reaches
      // MIN_DRAIN_BYTES, then drain once. `acc` only ever holds bytes that have
      // NOT yet been consumed (drainComplete drops the consumed prefix), so its
      // length is exactly the pending-but-undrained count. The header is the
      // one exception — decode it as soon as ≥5 bytes exist so small traces
      // don't stall waiting for a 256KB batch.
      if (acc.length < MIN_DRAIN_BYTES && headerDecoded) continue;

      drainBatch();

      if (onProgress) {
        onProgress({
          bytesRead: consumedBytes,
          // Total is unknown while streaming (we haven't seen the whole trace),
          // so report null rather than `consumedBytes + acc.length` — `acc` is
          // just the small undrained tail, which would peg any percentage at
          // ~99% the entire time.
          totalBytes: null,
          eventCount: state.events.length,
        });
        // Yield so the browser can paint the spinner, but only once at least
        // PAINT_INTERVAL_MS of wall-clock time has elapsed since the last
        // yield — not once per (tiny) chunk and not on a byte cadence. Because
        // the `for await` above already pumps the network/decompression, this
        // throttle reduces repaints without slowing download/parse overlap.
        const t = nowMs();
        if (t - lastYieldMs >= PAINT_INTERVAL_MS) {
          lastYieldMs = t;
          await new Promise((r) => setTimeout(r, 0));
        }
      }
    }

    // Drain the final partial batch: bytes that arrived after the last
    // MIN_DRAIN_BYTES drain (or, for a sub-256KB trace, the only batch).
    if (acc.length > 0 || (sawAnyBytes && !headerDecoded)) {
      drainBatch();
      if (onProgress) {
        onProgress({
          bytesRead: consumedBytes,
          totalBytes: null, // unknown while streaming — see note above
          eventCount: state.events.length,
        });
      }
    }

    if (!sawAnyBytes || !headerDecoded) {
      throw new Error("Invalid trace header");
    }

    // Stream ended. Any bytes still pending are a genuinely truncated tail
    // (the file ended mid-frame) — finalize with whatever we decoded, matching
    // the whole-buffer decoder's graceful EOF behavior.
    return finalizeParse(state, dec.version);
  }

  // ── Directory parsing (Node-only) ──

  /** Reconstruct Maps from [key, value] arrays produced by parse_worker.js. */
  function entriesToMap(arr) {
    return new Map(arr);
  }

  /** Load a cached ParsedTrace from NDJSON, reconstructing Maps. */
  async function loadCachedTrace(cachePath) {
    const fs = require('fs');
    const buf = await fs.promises.readFile(cachePath);
    let pos = 0;
    function nextLine() {
      const nl = buf.indexOf(10, pos);
      if (nl === -1) {
        if (pos < buf.length) { const s = buf.toString('utf8', pos, buf.length); pos = buf.length; return s; }
        return null;
      }
      const s = buf.toString('utf8', pos, nl);
      pos = nl + 1;
      return s;
    }

    let raw = null;
    const events = [];
    const cpuSamples = [];
    const customEvents = [];
    const allocEvents = [];
    const freeEvents = [];
    const memoryOverflows = [];

    let line;
    while ((line = nextLine()) !== null) {
      if (!line) continue;
      const rec = JSON.parse(line);
      switch (rec.t) {
        case 'm':
          raw = rec.d;
          if (raw.spawnLocations) raw.spawnLocations = entriesToMap(raw.spawnLocations);
          if (raw.taskSpawnLocs) raw.taskSpawnLocs = entriesToMap(raw.taskSpawnLocs);
          if (raw.taskSpawnTimes) raw.taskSpawnTimes = entriesToMap(raw.taskSpawnTimes);
          if (raw.taskTerminateTimes) raw.taskTerminateTimes = entriesToMap(raw.taskTerminateTimes);
          if (raw.callframeSymbols) raw.callframeSymbols = entriesToMap(raw.callframeSymbols);
          if (raw.threadNames) raw.threadNames = entriesToMap(raw.threadNames);
          if (raw.runtimeWorkers) raw.runtimeWorkers = entriesToMap(raw.runtimeWorkers);
          if (raw.taskDumps) raw.taskDumps = entriesToMap(raw.taskDumps);
          break;
        case 'e': events.push(rec.d); break;
        case 'c': cpuSamples.push(rec.d); break;
        case 'x': customEvents.push(rec.d); break;
        case 'a': allocEvents.push(rec.d); break;
        case 'f': freeEvents.push(rec.d); break;
        case 'o': memoryOverflows.push(rec.d); break;
      }
    }
    raw.events = events;
    raw.cpuSamples = cpuSamples;
    raw.customEvents = customEvents;
    raw.allocEvents = allocEvents;
    raw.freeEvents = freeEvents;
    raw.memoryOverflows = memoryOverflows;
    return raw;
  }

  /**
   * Parse all trace files in a directory with caching and parallelism.
   * Workers do parse + analysis. Cache holds pre-computed analysis results.
   * Returns {files, [Symbol.asyncIterator]} where each item is {file, analysis}.
   * @private
   */
  function parseTraceDir(dirPath, options) {
    const fs = require('fs');
    const path = require('path');
    const os = require('os');
    const { execFile } = require('child_process');

    const opts = options || {};
    const useCache = opts.cache !== false;
    const force = opts.force === true;
    const sampleN = opts.sample != null ? opts.sample : null;
    const onProgress = opts.onParseProgress || null;

    const TRACE_EXT = /\.(bin|bin\.gz)$/;
    let files = fs.readdirSync(dirPath)
      .filter(f => TRACE_EXT.test(f))
      .sort();

    if (files.length === 0) {
      throw new Error(`No .bin or .bin.gz files found in ${dirPath}`);
    }

    if (sampleN != null) {
      if (sampleN < 1) throw new Error('sample must be >= 1');
      if (sampleN < files.length) {
        const step = files.length / sampleN;
        const sampled = [];
        for (let i = 0; i < sampleN; i++) {
          sampled.push(files[Math.floor(i * step)]);
        }
        files = sampled;
      }
    }

    const cacheDir = path.join(dirPath, '.d9-cache');
    if (useCache) {
      fs.mkdirSync(cacheDir, { recursive: true });
    }

    const concurrency = (opts.parallel === false) ? 1 : Math.min(os.cpus().length, 32);
    const workerCandidate = path.resolve(__dirname, 'analyze.js');
    const workerFallback = path.resolve(__dirname, '..', 'skills', 'dial9-toolkit', 'scripts', 'analyze.js');
    const workerScript = fs.existsSync(workerCandidate) ? workerCandidate : workerFallback;

    function cachePathFor(file) {
      return path.join(cacheDir, file.replace(TRACE_EXT, '') + '.json');
    }

    function isCacheValid(file) {
      if (!useCache || force) return false;
      const cp = cachePathFor(file);
      try {
        const cacheStat = fs.statSync(cp);
        const srcStat = fs.statSync(path.join(dirPath, file));
        return cacheStat.mtimeMs > srcStat.mtimeMs;
      } catch { return false; }
    }

    // Ensure file is cached (spawn worker if needed). Returns Promise<boolean> (true = cache hit).
    function ensureCached(file) {
      if (isCacheValid(file)) return Promise.resolve(true);
      const tracePath = path.join(dirPath, file);
      const cp = useCache ? cachePathFor(file) : path.join(os.tmpdir(), 'd9-' + process.pid + '-' + file + '.json');
      const args = [workerScript, '--parse-worker', tracePath, cp];
      return new Promise((resolve, reject) => {
        execFile(process.execPath, args, { maxBuffer: 10 * 1024 * 1024 }, (err, stdout, stderr) => {
          if (err) reject(new Error(`Failed to process ${file}: ${stderr || err.message}`));
          else resolve(false);
        });
      });
    }

    // Dispatch all workers with concurrency limiting.
    // Workers run independently of the iterator.
    if (onProgress) onProgress({ done: 0, total: files.length, file: null });

    let workersCompleted = 0;
    let cacheHits = 0;
    const fileReady = [];
    let active = 0;
    const waiters = [];

    for (let i = 0; i < files.length; i++) {
      fileReady.push(new Promise((resolve, reject) => {
        function go() {
          active++;
          ensureCached(files[i]).then((wasCached) => {
            workersCompleted++;
            if (wasCached) cacheHits++;
            active--;
            if (onProgress) onProgress({ done: workersCompleted, total: files.length, file: files[i], cached: cacheHits });
            resolve();
            if (waiters.length > 0) waiters.shift()();
          }, reject);
        }
        if (active < concurrency) go();
        else waiters.push(go);
      }));
    }

    return {
      files: files,
      allCached: Promise.all(fileReady),
      [Symbol.asyncIterator]() {
        let idx = 0;
        return {
          async next() {
            if (idx >= files.length) return { done: true, value: undefined };
            const i = idx++;
            await fileReady[i];
            const cp = useCache ? cachePathFor(files[i]) : path.join(os.tmpdir(), 'd9-' + process.pid + '-' + files[i] + '.json');
            const trace = await loadCachedTrace(cp);
            if (!useCache) try { fs.unlinkSync(cp); } catch {}
            return { done: false, value: trace };
          }
        };
      }
    };
  }

  // ── Symbol formatting utilities ──

  function _stripBoringGenerics(s) {
    const boring = /^[A-Z]$|^(Fut|Req|Res|Bs|InnerFuture)$/;
    return s.replace(/<([^<>]*)>/g, (match, inner) => {
      const params = inner.split(",").map((p) => p.trim());
      if (params.every((p) => boring.test(p))) return "";
      const kept = params.filter((p) => !boring.test(p));
      return kept.length ? `<${kept.join(",")}>` : "";
    });
  }

  function _lastSeg(s) {
    return s.split("::").pop();
  }

  function _shortenPath(s) {
    const parts = s.split("::");
    let closures = 0;
    for (let i = parts.length - 1; i >= 0; i--) {
      if (parts[i] === "{{closure}}") closures++;
      else break;
    }
    const meaningful = parts.length - closures;
    if (meaningful <= 3) return s;
    return parts.slice(meaningful - 3).join("::");
  }

  /**
   * Try to build a docs.rs source link from a location path containing a crate-version segment.
   * Matches any path like: .../hyper-0.14.28/src/client/connect/http.rs:474
   * Returns URL string or null.
   */
  function _docsRsUrl(location) {
    if (!location) return null;
    const m = location.match(
      /\/([a-z][a-z0-9_-]*)-(\d+\.\d+[^/]*)\/(.+?)(?::(\d+))?$/
    );
    if (!m) return null;
    const [, crate_, version, rawPath, line] = m;
    const crateSrc = crate_.replace(/-/g, "_");
    const path = rawPath.replace(/^src\//, "");
    let url = `https://docs.rs/${crate_}/${version}/src/${crateSrc}/${path}.html`;
    if (line) url += `#${line}`;
    return url;
  }

  /**
   * Extract just the filename from a location string.
   * e.g. "/home/user/.cargo/registry/src/.../hyper-0.14.28/src/client/connect/http.rs:474" → "http.rs"
   */
  function _fileName(location) {
    if (!location) return null;
    const m = location.match(/([^/]+\.rs)(?::\d+)?$/);
    return m ? m[1] : null;
  }

  /**
   * Format a stack frame for human-readable display.
   * Accepts either a resolved frame object or a raw address + callframeSymbols map.
   * @param {{symbol: string, location: string|null}|string} frame - Resolved frame or address string
   * @param {Map<string, {symbol: string, location: string|null}>} [callframeSymbols] - Required when frame is an address string
   * @returns {{text: string, docsUrl: string|null}}
   */
  function formatFrame(frame, callframeSymbols) {
    if (typeof frame === "string") {
      if (!callframeSymbols)
        throw new Error(
          "formatFrame requires callframeSymbols when given an address string"
        );
      const entry = callframeSymbols.get(frame);
      if (!entry) return { text: frame || "(unknown)", docsUrl: null };
      frame = Array.isArray(entry) ? entry[0] : entry;
    }
    const { symbol: sym, location } = frame;
    if (!sym || sym.startsWith("0x"))
      return { text: sym || "(unknown)", docsUrl: null };

    let result = sym;
    const traitImplMatch = result.match(/^<(.+?) as (.+?)>::(.+)$/);
    if (traitImplMatch) {
      let [, implType, trait_, method] = traitImplMatch;
      const shortType = _lastSeg(_stripBoringGenerics(implType));
      result =
        shortType.length <= 2
          ? `${_lastSeg(_stripBoringGenerics(trait_))}::${method}`
          : `${shortType}::${method}`;
    } else if (result.includes("::")) {
      result = _shortenPath(_stripBoringGenerics(result));
    }

    const fileName = _fileName(location);
    if (location) {
      const m = location.match(/:(\d+)$/);
      if (m) result += ` ${fileName || ""}:${m[1]}`;
    }
    return { text: result, docsUrl: _docsRsUrl(location) };
  }

  /**
   * Resolve a callchain (array of address strings) to frame objects.
   * When an address has inlined frames (stored as an array in callframeSymbols),
   * they are expanded in place (outermost first, then inlined callees).
   * @param {string[]} callchain - Address strings like "0x55cc6d053893"
   * @param {Map<string, {symbol: string, location: string|null}|Array>} callframeSymbols
   * @returns {{symbol: string, location: string|null}[]}
   */
  function symbolizeChain(callchain, callframeSymbols) {
    const result = [];
    for (const addr of callchain) {
      const entry = callframeSymbols.get(addr);
      if (!entry) {
        result.push({ symbol: addr, location: null });
        continue;
      }
      if (Array.isArray(entry)) {
        for (const e of entry) result.push(e);
        continue;
      }
      if (typeof entry === "string") {
        result.push({ symbol: entry, location: null });
        continue;
      }
      result.push(entry);
    }
    return result;
  }

  /**
   * Deduplicate CPU/sched samples by symbolized stack trace.
   * @param {Object[]} samples - Array of {callchain, ...} sample objects
   * @param {Map} callframeSymbols
   * @returns {{count: number, frames: Object[], leaf: string, leafRaw: string}[]}
   */
  function deduplicateSamples(samples, callframeSymbols) {
    const groups = new Map();
    for (const sample of samples) {
      const frames = symbolizeChain(sample.callchain, callframeSymbols);
      const key = frames.map((f) => f.symbol).join("\0");
      if (!groups.has(key)) {
        groups.set(key, {
          count: 0,
          frames,
          leaf: frames[0] ? formatFrame(frames[0]).text : "(unknown)",
          leafRaw: frames[0] ? frames[0].symbol : "",
        });
      }
      groups.get(key).count++;
    }
    return [...groups.values()].sort((a, b) => b.count - a.count);
  }

  /**
   * Parse a single trace file and return the ParsedTrace directly.
   * Accepts a file path (string) or a Buffer. Always returns Promise<ParsedTrace>.
   */
  async function parseOne(input, options) {
    if (typeof input === 'string' && typeof require !== 'undefined') {
      const fs = require('fs');
      const stat = fs.statSync(input);
      if (stat.isDirectory()) {
        throw new Error('parseOne expects a single file, not a directory. Use parseTrace for directories.');
      }
      return parseTraceBuffer(fs.readFileSync(input), options);
    }
    return parseTraceBuffer(input, options);
  }

  // Export for both browser and Node.js
  if (typeof module !== "undefined" && module.exports) {
    module.exports = {
      EVENT_TYPES,
      OFF_WORKER_WORKER_ID,
      parseTrace,
      parseTraceStream,
      parseOne,
      fetchTraces,
      fetchTraceStream,
      canStreamDecode,
      formatFrame,
      symbolizeChain,
      deduplicateSamples,
      deriveBlockInPlaceGaps,
    };
  } else {
    exports.TraceParser = {
      EVENT_TYPES,
      OFF_WORKER_WORKER_ID,
      parseTrace,
      parseTraceStream,
      fetchTraces,
      fetchTraceStream,
      canStreamDecode,
      formatFrame,
      symbolizeChain,
      deduplicateSamples,
      deriveBlockInPlaceGaps,
    };
  }
})(typeof exports === "undefined" ? this : exports);
