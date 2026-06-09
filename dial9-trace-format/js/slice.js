// dial9-trace-format slicer — filters a trace by time range.
// See SPEC.md for the binary format specification.
//
// Strategy: walk the raw bytes frame-by-frame using the decoder logic to
// determine frame boundaries and timestamps. Copy non-event frames verbatim.
// For event frames, decode only enough to check the timestamp, then either
// copy the raw bytes or skip.
//
// This avoids needing a full encoder — we never re-encode field values.

"use strict";

const { TraceDecoder, FieldType } = require("./decode.js");

const MAGIC = [0x54, 0x52, 0x43, 0x00];
const TAG_SCHEMA = 0x01;
const TAG_EVENT = 0x02;
const TAG_STRING_POOL = 0x03;
const TAG_STACK_POOL = 0x04;
const TAG_TIMESTAMP_RESET = 0x05;
const TAG_SCHEMA_ANNOTATIONS = 0x06;

const OPTIONAL_BIT = 0x80;

// Events that describe the trace itself and must survive time-range filtering.
// Without these, the parser cannot resolve CPU sample addresses to symbols or
// anchor monotonic timestamps to wall-clock time.
const ALWAYS_KEEP_SCHEMA_NAMES = new Set([
  "SymbolTableEntry",
  "SegmentMetadataEvent",
  "ClockSyncEvent",
]);

/**
 * Slice a trace buffer, keeping only events within the given time range.
 *
 * @param {Buffer|Uint8Array} input - Raw trace bytes (gzipped or raw)
 * @param {Object} [opts]
 * @param {Object} [opts.timeRange] - { startNs, endNs } monotonic ns bounds (inclusive)
 * @param {boolean} [opts.relative] - If true, startNs/endNs are offsets from the trace's first timestamp
 * @returns {Buffer} - The sliced trace bytes (raw, not gzipped)
 */
function sliceTrace(input, opts) {
  const buf = maybeGunzipSync(input);
  const timeRange = opts && opts.timeRange;
  if (!timeRange) {
    // No filter — return as-is
    return Buffer.from(buf);
  }

  let startNs = BigInt(timeRange.startNs);
  let endNs = BigInt(timeRange.endNs);

  if (opts && opts.relative) {
    const minTs = findMinTs(buf);
    startNs = minTs + startNs;
    endNs = minTs + endNs;
  }

  const view = new DataView(buf.buffer, buf.byteOffset, buf.byteLength);
  const out = [];
  let pos = 0;

  // Schemas we've seen (need to know hasTimestamp + field layout to skip event bodies)
  const schemas = new Map();
  let timestampBaseNs = 0n;

  // We need to emit timestamp resets before kept events so the output decoder
  // can reconstruct absolute timestamps. Track what the output decoder's base is.
  let outputTimestampBaseNs = 0n;

  function copyBytes(start, end) {
    out.push(buf.slice(start, end));
  }

  // Validate and copy header
  if (buf.length < 5) throw new Error("Trace too short for header");
  for (let i = 0; i < 4; i++) {
    if (buf[i] !== MAGIC[i]) throw new Error("Invalid trace magic");
  }
  const version = buf[4];
  if (version < 1 || version > 127) throw new Error(`Unsupported trace version: ${version}`);
  copyBytes(0, 5);
  pos = 5;

  while (pos < buf.length) {
    const frameStart = pos;
    const tag = view.getUint8(pos);

    // Mid-stream header (concatenated segments)
    if (tag === MAGIC[0] && pos + 5 <= buf.length) {
      let isHeader = true;
      for (let i = 1; i < 4; i++) {
        if (view.getUint8(pos + i) !== MAGIC[i]) { isHeader = false; break; }
      }
      if (isHeader) {
        schemas.clear();
        timestampBaseNs = 0n;
        outputTimestampBaseNs = 0n;
        copyBytes(frameStart, frameStart + 5);
        pos += 5;
        continue;
      }
    }

    pos++; // consume tag

    switch (tag) {
      case TAG_SCHEMA: {
        const schemaStart = pos;
        const typeId = view.getUint16(pos, true); pos += 2;
        const nameLen = view.getUint16(pos, true); pos += 2;
        const name = Buffer.from(buf.slice(pos, pos + nameLen)).toString("utf8");
        pos += nameLen;
        const hasTimestamp = view.getUint8(pos) !== 0; pos += 1;
        const fieldCount = view.getUint16(pos, true); pos += 2;
        const fields = [];
        for (let i = 0; i < fieldCount; i++) {
          const fnLen = view.getUint16(pos, true); pos += 2;
          pos += fnLen; // skip field name
          const ft = view.getUint8(pos); pos++;
          fields.push(ft);
        }
        schemas.set(typeId, { name, hasTimestamp, fields });
        // Always copy schema frames
        copyBytes(frameStart, pos);
        break;
      }

      case TAG_EVENT: {
        const typeId = view.getUint16(pos, true); pos += 2;
        const schema = schemas.get(typeId);
        if (!schema) throw new Error(`Unknown type_id ${typeId} at offset ${frameStart}`);

        let eventTimestampNs = null;
        if (schema.hasTimestamp) {
          const b0 = view.getUint8(pos);
          const b1 = view.getUint8(pos + 1);
          const b2 = view.getUint8(pos + 2);
          const deltaNs = b0 | (b1 << 8) | (b2 << 16);
          pos += 3;
          eventTimestampNs = timestampBaseNs + BigInt(deltaNs);
          timestampBaseNs = eventTimestampNs;
        }

        // Skip over field values to find end of frame
        for (const ft of schema.fields) {
          pos += fieldSize(view, pos, ft);
        }

        // Decide whether to keep this event
        let keep = true;
        if (eventTimestampNs !== null) {
          if (eventTimestampNs < startNs || eventTimestampNs > endNs) {
            keep = false;
          }
        }
        // Always retain trace-describing events (symbols, metadata, clock sync)
        // so the parser can resolve addresses and anchor timestamps.
        if (!keep && ALWAYS_KEEP_SCHEMA_NAMES.has(schema.name)) {
          keep = true;
        }
        // Events without timestamps (rare) are always kept

        if (keep) {
          // We need to emit a timestamp reset if the output decoder's base
          // doesn't match what this event expects
          if (schema.hasTimestamp && eventTimestampNs !== null) {
            const outputDelta = eventTimestampNs - outputTimestampBaseNs;
            if (outputDelta < 0n || outputDelta > 16777215n) {
              // Emit a timestamp reset frame
              const resetBuf = Buffer.alloc(9);
              resetBuf[0] = TAG_TIMESTAMP_RESET;
              resetBuf.writeBigUInt64LE(eventTimestampNs, 1);
              out.push(resetBuf);
              outputTimestampBaseNs = eventTimestampNs;
            }
            // Rewrite the event's delta relative to outputTimestampBaseNs
            const newDelta = Number(eventTimestampNs - outputTimestampBaseNs);
            const eventBuf = Buffer.from(buf.slice(frameStart, pos));
            // The delta is at offset: 1 (tag) + 2 (type_id) = 3
            eventBuf[3] = newDelta & 0xFF;
            eventBuf[4] = (newDelta >> 8) & 0xFF;
            eventBuf[5] = (newDelta >> 16) & 0xFF;
            out.push(eventBuf);
            outputTimestampBaseNs = eventTimestampNs;
          } else {
            copyBytes(frameStart, pos);
          }
        }
        break;
      }

      case TAG_STRING_POOL: {
        const count = view.getUint32(pos, true); pos += 4;
        for (let i = 0; i < count; i++) {
          pos += 4; // pool_id
          const len = view.getUint32(pos, true); pos += 4;
          pos += len;
        }
        copyBytes(frameStart, pos);
        break;
      }

      case TAG_STACK_POOL: {
        const count = view.getUint32(pos, true); pos += 4;
        for (let i = 0; i < count; i++) {
          pos += 4; // pool_id
          const frameCount = view.getUint32(pos, true); pos += 4;
          pos += frameCount * 8;
        }
        copyBytes(frameStart, pos);
        break;
      }

      case TAG_TIMESTAMP_RESET: {
        const lo = view.getUint32(pos, true);
        const hi = view.getUint32(pos + 4, true);
        timestampBaseNs = (BigInt(hi) << 32n) | BigInt(lo);
        pos += 8;
        // Don't copy — we emit our own resets as needed
        break;
      }

      case TAG_SCHEMA_ANNOTATIONS: {
        // Schema annotations: varint type_id + u16 count + entries
        // We always keep these. Parse just enough to find the end.
        const [_typeId, varintSize] = decodeULEB128(view, pos);
        pos += varintSize;
        const annCount = view.getUint16(pos, true); pos += 2;
        for (let i = 0; i < annCount; i++) {
          pos += 2; // field_index
          const keyLen = view.getUint16(pos, true); pos += 2;
          pos += keyLen;
          const valLen = view.getUint32(pos, true); pos += 4;
          pos += valLen;
        }
        copyBytes(frameStart, pos);
        break;
      }

      default:
        throw new Error(`Unknown frame tag 0x${tag.toString(16)} at offset ${frameStart}`);
    }
  }

  return Buffer.concat(out);
}

/** Compute the wire size of a field value without decoding it fully. */
function fieldSize(view, offset, fieldType) {
  if (fieldType & OPTIONAL_BIT) {
    const prefix = view.getUint8(offset);
    if (prefix === 0x00) return 1;
    return 1 + fieldSize(view, offset + 1, fieldType & 0x7F);
  }
  switch (fieldType) {
    case FieldType.I64: return 8;
    case FieldType.F64: return 8;
    case FieldType.Bool: return 1;
    case FieldType.U8: return 1;
    case FieldType.U16: return 2;
    case FieldType.U32: return 4;
    case FieldType.PooledString: return 4;
    case FieldType.PooledStackFrames: return 4;
    case FieldType.String:
    case FieldType.Bytes: {
      const len = view.getUint32(offset, true);
      return 4 + len;
    }
    case FieldType.Varint: {
      const [, consumed] = decodeULEB128(view, offset);
      return consumed;
    }
    case FieldType.StackFrames: {
      const count = view.getUint32(offset, true);
      return 4 + count * 8;
    }
    case FieldType.StringMap: {
      const count = view.getUint32(offset, true);
      let size = 4;
      for (let i = 0; i < count; i++) {
        const kLen = view.getUint32(offset + size, true); size += 4 + kLen;
        const vLen = view.getUint32(offset + size, true); size += 4 + vLen;
      }
      return size;
    }
    case FieldType.DynamicList: {
      const count = view.getUint32(offset, true);
      let size = 4;
      for (let i = 0; i < count; i++) {
        const tag = view.getUint8(offset + size); size += 1;
        size += fieldSize(view, offset + size, tag);
      }
      return size;
    }
    case FieldType.DynamicMap: {
      const count = view.getUint32(offset, true);
      let size = 4;
      for (let i = 0; i < count; i++) {
        const keyTag = view.getUint8(offset + size); size += 1;
        size += fieldSize(view, offset + size, keyTag);
        const valTag = view.getUint8(offset + size); size += 1;
        size += fieldSize(view, offset + size, valTag);
      }
      return size;
    }
    default:
      throw new Error(`Unknown field type: ${fieldType}`);
  }
}

function decodeULEB128(view, offset) {
  let result = 0n;
  let shift = 0n;
  let pos = offset;
  while (true) {
    const byte = view.getUint8(pos++);
    result |= BigInt(byte & 0x7f) << shift;
    shift += 7n;
    if ((byte & 0x80) === 0) return [result, pos - offset];
  }
}

function maybeGunzipSync(input) {
  const b = input instanceof Uint8Array ? input : new Uint8Array(input);
  if (b.length >= 2 && b[0] === 0x1f && b[1] === 0x8b) {
    const zlib = require("zlib");
    const decompressed = zlib.gunzipSync(Buffer.from(b));
    return new Uint8Array(decompressed.buffer, decompressed.byteOffset, decompressed.byteLength);
  }
  return b;
}

/**
 * Find the first event's absolute timestamp (minTs) by scanning the trace.
 * Walks frames until the first timestamped event is found.
 */
function findMinTs(buf) {
  const view = new DataView(buf.buffer, buf.byteOffset, buf.byteLength);
  const schemas = new Map();
  let timestampBaseNs = 0n;
  let pos = 5; // skip header (magic + version)

  while (pos < buf.length) {
    const tag = view.getUint8(pos);

    // Mid-stream header
    if (tag === MAGIC[0] && pos + 5 <= buf.length) {
      let isHeader = true;
      for (let i = 1; i < 4; i++) {
        if (view.getUint8(pos + i) !== MAGIC[i]) { isHeader = false; break; }
      }
      if (isHeader) { schemas.clear(); timestampBaseNs = 0n; pos += 5; continue; }
    }

    pos++; // consume tag

    switch (tag) {
      case TAG_SCHEMA: {
        const typeId = view.getUint16(pos, true); pos += 2;
        const nameLen = view.getUint16(pos, true); pos += 2;
        pos += nameLen;
        const hasTimestamp = view.getUint8(pos) !== 0; pos += 1;
        const fieldCount = view.getUint16(pos, true); pos += 2;
        const fields = [];
        for (let i = 0; i < fieldCount; i++) {
          const fnLen = view.getUint16(pos, true); pos += 2;
          pos += fnLen;
          const ft = view.getUint8(pos); pos++;
          fields.push(ft);
        }
        schemas.set(typeId, { hasTimestamp, fields });
        break;
      }
      case TAG_EVENT: {
        const typeId = view.getUint16(pos, true); pos += 2;
        const schema = schemas.get(typeId);
        if (!schema) throw new Error(`Unknown type_id ${typeId} in findMinTs`);
        if (schema.hasTimestamp) {
          const b0 = view.getUint8(pos);
          const b1 = view.getUint8(pos + 1);
          const b2 = view.getUint8(pos + 2);
          const deltaNs = b0 | (b1 << 8) | (b2 << 16);
          return timestampBaseNs + BigInt(deltaNs);
        }
        // Skip fields to continue searching
        pos += schema.hasTimestamp ? 3 : 0;
        for (const ft of schema.fields) { pos += fieldSize(view, pos, ft); }
        break;
      }
      case TAG_TIMESTAMP_RESET: {
        const lo = view.getUint32(pos, true);
        const hi = view.getUint32(pos + 4, true);
        timestampBaseNs = (BigInt(hi) << 32n) | BigInt(lo);
        pos += 8;
        break;
      }
      case TAG_STRING_POOL: {
        const count = view.getUint32(pos, true); pos += 4;
        for (let i = 0; i < count; i++) {
          pos += 4;
          const len = view.getUint32(pos, true); pos += 4;
          pos += len;
        }
        break;
      }
      case TAG_STACK_POOL: {
        const count = view.getUint32(pos, true); pos += 4;
        for (let i = 0; i < count; i++) {
          pos += 4;
          const frameCount = view.getUint32(pos, true); pos += 4;
          pos += frameCount * 8;
        }
        break;
      }
      case TAG_SCHEMA_ANNOTATIONS: {
        const [_typeId, varintSize] = decodeULEB128(view, pos);
        pos += varintSize;
        const annCount = view.getUint16(pos, true); pos += 2;
        for (let i = 0; i < annCount; i++) {
          pos += 2;
          const keyLen = view.getUint16(pos, true); pos += 2;
          pos += keyLen;
          const valLen = view.getUint32(pos, true); pos += 4;
          pos += valLen;
        }
        break;
      }
      default:
        throw new Error(`Unknown frame tag 0x${tag.toString(16)} in findMinTs`);
    }
  }
  throw new Error("No timestamped event found in trace");
}

// --- CLI ---
if (require.main === module) {
  const fs = require("fs");
  const args = process.argv.slice(2);
  let inputPath, outputPath, startNs, endNs, relative = false;
  for (let i = 0; i < args.length; i++) {
    switch (args[i]) {
      case "--input": inputPath = args[++i]; break;
      case "--output": outputPath = args[++i]; break;
      case "--start": startNs = args[++i]; break;
      case "--end": endNs = args[++i]; break;
      case "--relative": relative = true; break;
      default:
        console.error(`Unknown arg: ${args[i]}`);
        process.exit(1);
    }
  }
  if (!inputPath || !outputPath) {
    console.error(`Usage: node slice.js --input <trace.bin> --output <sliced.bin> [--start <ns>] [--end <ns>] [--relative]

Options:
  --input <path>    Input trace file (required)
  --output <path>   Output sliced trace file (required)
  --start <ns>      Start timestamp in nanoseconds (inclusive)
  --end <ns>        End timestamp in nanoseconds (inclusive)
  --relative        Interpret --start/--end as offsets from trace start (minTs).
                    Without this flag, values are absolute monotonic ns (matching event.ts).
                    Use --relative when your timestamps come from analyze.js or similar
                    tools that report time relative to trace start.

Examples:
  # Absolute (event.ts values from parseTrace — typically 10-15 digit numbers):
  node slice.js --input full.bin --output burst.bin --start 150439548276 --end 154676563153

  # Relative (offsets from trace start — typically 9-10 digit numbers):
  node slice.js --input full.bin --output burst.bin --relative --start 3900000000 --end 4050000000`);
    process.exit(1);
  }
  if (startNs != null && endNs != null && BigInt(startNs) > BigInt(endNs)) {
    console.error("Error: --start must be <= --end");
    process.exit(1);
  }
  const input = fs.readFileSync(inputPath);
  const opts = {};
  if (startNs != null || endNs != null) {
    opts.timeRange = {
      startNs: startNs != null ? startNs : "0",
      endNs: endNs != null ? endNs : "18446744073709551615",
    };
  }
  if (relative) {
    opts.relative = true;
  }
  const result = sliceTrace(input, opts);
  fs.writeFileSync(outputPath, result);
  console.log(`Sliced ${inputPath} (${input.length} bytes) -> ${outputPath} (${result.length} bytes)`);
}

module.exports = { sliceTrace };
