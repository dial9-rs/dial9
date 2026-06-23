// trace_properties.js - Canonical, language-neutral "trace properties" extractor.
//
// This is the *reference oracle* for the CPU-event decode pipeline. The Rust
// decoder (`dial9-viewer/src/ingest/decode.rs`) is being brought to parity with
// the JS reference parser (`trace_parser.js`), and the only way to trust the
// Rust port is to run the SAME trace bytes through both and compare a fixed set
// of properties. This module computes those properties from a `ParsedTrace`.
//
// The properties are deliberately chosen to be:
//
//   * Decoder-independent — defined in terms of observable sample attributes
//     (source, worker attribution, symbolized stack), never internal layout.
//   * Offset-invariant — timestamps are compared by their *deltas from the
//     minimum*, so the JS parser (monotonic ns) and the Rust decoder (wall-clock
//     ns, after applying the ClockSync offset) digest identically. A constant
//     additive offset cancels: (t+k) - min(t+k) == t - min(t).
//   * Source-aware — CPU profiling samples (`source = CpuProfile`) and scheduler
//     context-switch samples (`source = SchedEvent`) are fundamentally different
//     kinds of data. The viewer's on-CPU flamegraph shows ONLY CpuProfile
//     samples (see `trace_analysis.js` `isCpuProfileSample`, `flamegraph.js`).
//     Conflating the two yields a flamegraph dominated by `schedule` frames.
//     Every count here is broken out per source so any conflation is visible.
//
// The digests use FNV-1a (64-bit) over a canonical serialization so the exact
// same algorithm is trivially reproducible in Rust (u64 wrapping arithmetic).
//
// CLI:  node trace_properties.js <trace-file>   # prints the properties as JSON

(function (exports) {
  "use strict";

  function getParser() {
    if (typeof require !== "undefined") return require("./trace_parser.js");
    if (typeof TraceParser !== "undefined") return TraceParser;
    throw new Error("TraceParser not found. Load trace_parser.js first.");
  }

  // Sentinel worker id for samples not attributable to a runtime worker.
  // Mirrors trace_parser.js `OFF_WORKER_WORKER_ID` / producer `WorkerId::UNKNOWN`.
  const OFF_WORKER_WORKER_ID = 255;

  // Worker ids at or above this are producer sentinels, not real runtime
  // workers: 254 = `WorkerId::BLOCKING`, 255 = `WorkerId::UNKNOWN`. A "real"
  // worker — the set the Rust `Option<worker_id>` carries as `Some` — is any id
  // below this; samples on a sentinel/unmapped tid are off-runtime.
  const FIRST_SENTINEL_WORKER_ID = 254;

  // Canonical CPU-sample source codes (wire values), see `CpuSampleSource`.
  const SOURCE_CPU_PROFILE = 0; // periodic on-CPU profiling sample
  const SOURCE_SCHED_EVENT = 1; // context switch (off-CPU); excluded from flamegraph

  // Frame separator for stack signatures. MUST be a byte that never appears in
  // a symbol name: Rust symbol names routinely contain spaces (e.g.
  // "<T as Trait>::method"), so a space separator would collide distinct
  // stacks. NUL (0x00) is the separator the Rust decoder itself hashes with
  // (`decode.rs` uses b"\x00"). Written as an explicit \u escape so it can never
  // silently become an invisible byte in the source.
  const FRAME_SEP = String.fromCharCode(0); // NUL (0x00)

  // ── FNV-1a 64-bit over bytes, BigInt arithmetic (matches Rust u64 wrapping) ──
  const FNV_OFFSET = 0xcbf29ce484222325n;
  const FNV_PRIME = 0x100000001b3n;
  const MASK64 = 0xffffffffffffffffn;

  function fnv1aInit() {
    return FNV_OFFSET;
  }
  function fnv1aUpdateStr(h, s) {
    // Hash the UTF-8 bytes of `s`.
    const bytes = Buffer.from(s, "utf8");
    for (let i = 0; i < bytes.length; i++) {
      h = (h ^ BigInt(bytes[i])) & MASK64;
      h = (h * FNV_PRIME) & MASK64;
    }
    return h;
  }
  function fnv1aHex(h) {
    return h.toString(16).padStart(16, "0");
  }

  /**
   * The symbolized frame-name signature of a sample's callchain: the sequence
   * of frame symbol names (inlines expanded, outermost→inlined), joined by
   * {@link FRAME_SEP}. Hash-independent (no blake3 needed) and directly
   * comparable to the Rust side, which reconstructs the same names from its
   * stacks dictionary.
   */
  function stackSignature(sample, callframeSymbols, symbolizeChain) {
    const frames = symbolizeChain(sample.callchain, callframeSymbols);
    return frames.map((f) => f.symbol).join(FRAME_SEP);
  }

  /**
   * Compute the canonical TraceProperties for a parsed trace.
   *
   * Universe: samples with a non-empty callchain. Both decoders drop empty
   * callchains, so this is the shared denominator.
   *
   * @param {object} trace ParsedTrace from trace_parser.parseTrace
   * @returns {object} JSON-serializable properties (see module doc)
   */
  function computeProperties(trace) {
    const { symbolizeChain } = getParser();
    const callframeSymbols = trace.callframeSymbols;
    const samples = trace.cpuSamples.filter((s) => s.callchain.length > 0);

    const bySource = {};
    const onOffBySource = {};
    let onRuntime = 0;
    let offRuntime = 0;
    const workerSet = new Set();
    const sigCounts = new Map(); // signature -> count (over CpuProfile samples)
    const deltas = []; // (ts - minTs) over CpuProfile samples, offset-invariant

    let minTs = null;
    let maxTs = null;
    for (const s of samples) {
      // Only CpuProfile samples define the on-CPU timestamp series and the
      // flamegraph stack set; sched samples are a separate (off-CPU) series.
      if (s.source === SOURCE_CPU_PROFILE) {
        if (minTs === null || s.timestamp < minTs) minTs = s.timestamp;
        if (maxTs === null || s.timestamp > maxTs) maxTs = s.timestamp;
      }
    }

    for (const s of samples) {
      const src = String(s.source);
      bySource[src] = (bySource[src] || 0) + 1;

      // On-runtime iff attributed to a real worker. A sentinel/unmapped id
      // (>= FIRST_SENTINEL_WORKER_ID, or the 255 off-worker value) is off. This
      // is exactly `Some` vs `None` on the Rust side (`Option<worker_id>`).
      const isOn = s.workerId < FIRST_SENTINEL_WORKER_ID;
      if (isOn) {
        onRuntime++;
        workerSet.add(s.workerId); // real workers only — matches Rust's Some set
      } else {
        offRuntime++;
      }

      if (!onOffBySource[src]) onOffBySource[src] = { on: 0, off: 0 };
      onOffBySource[src][isOn ? "on" : "off"]++;

      if (s.source === SOURCE_CPU_PROFILE) {
        const sig = stackSignature(s, callframeSymbols, symbolizeChain);
        sigCounts.set(sig, (sigCounts.get(sig) || 0) + 1);
        deltas.push(s.timestamp - minTs);
      }
    }

    // Order-independent digest of the (signature -> count) multiset.
    let sigHash = fnv1aInit();
    for (const sig of [...sigCounts.keys()].sort()) {
      sigHash = fnv1aUpdateStr(sigHash, sig);
      sigHash = fnv1aUpdateStr(sigHash, "" + sigCounts.get(sig) + "\n");
    }

    // Offset-invariant digest of the sorted CpuProfile timestamp deltas.
    deltas.sort((a, b) => a - b);
    let tsHash = fnv1aInit();
    for (const d of deltas) tsHash = fnv1aUpdateStr(tsHash, d + "\n");

    // Human-readable top stacks (CpuProfile only).
    const topStacks = [...sigCounts.entries()]
      .sort((a, b) => b[1] - a[1])
      .slice(0, 8)
      .map(([sig, count]) => {
        const names = sig.split(FRAME_SEP);
        return { count, depth: names.length, leaf: names[0] || "(empty)" };
      });

    return {
      universe: "callchain_nonempty",
      total_samples: samples.length,
      by_source: bySource,
      on_off_all: { on_runtime: onRuntime, off_runtime: offRuntime },
      on_off_by_source: onOffBySource,
      worker_set: [...workerSet].sort((a, b) => a - b),
      cpu_profile: {
        count: bySource[String(SOURCE_CPU_PROFILE)] || 0,
        distinct_stacks: sigCounts.size,
        stack_sig_digest: fnv1aHex(sigHash),
        ts_count: deltas.length,
        ts_span_ns: minTs === null ? 0 : maxTs - minTs,
        ts_delta_digest: fnv1aHex(tsHash),
        top_stacks: topStacks,
      },
    };
  }

  async function computePropertiesFromFile(tracePath) {
    const { parseTrace } = getParser();
    const trace = await parseTrace(tracePath);
    return computeProperties(trace);
  }

  if (typeof module !== "undefined" && module.exports) {
    module.exports = {
      computeProperties,
      computePropertiesFromFile,
      FRAME_SEP,
      OFF_WORKER_WORKER_ID,
      SOURCE_CPU_PROFILE,
      SOURCE_SCHED_EVENT,
    };
  } else {
    exports.TraceProperties = { computeProperties, computePropertiesFromFile };
  }

  // CLI entry point: print JSON properties for a trace file.
  if (typeof require !== "undefined" && require.main === module) {
    const tracePath = process.argv[2];
    if (!tracePath) {
      console.error("usage: node trace_properties.js <trace-file>");
      process.exit(2);
    }
    computePropertiesFromFile(tracePath)
      .then((props) => {
        process.stdout.write(JSON.stringify(props, null, 2) + "\n");
      })
      .catch((e) => {
        console.error("error:", e.message);
        process.exit(1);
      });
  }
})(typeof exports === "undefined" ? this : exports);
