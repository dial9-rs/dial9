// Hand-written type declarations for the frozen-core file `decode.js`
// (dial9-trace-format decoder). See docs/adr/0004-viewer-ui-migration.md
// section 6 (frozen core policy) and 02-architecture.md sections 2.6/2.7.
//
// Declaration form: an ambient wildcard module (`*/decode.js`) so any
// relative import path from src/ (e.g. `import { TraceDecoder } from
// "../../decode.js"`) picks up these types via Vite's CJS interop without
// enabling allowJs. The wildcard matches the trailing `/decode.js` segment
// of the specifier; the core .js file itself is never type-checked.
//
// Public API only; shapes verified against decode.js (whole file) and its
// consumers (trace_parser.js getTraceDecoder/parse loops, test_stream_parse.js).

declare module "*/decode.js" {
  /**
   * Wire field-type ids used in event schemas. The high bit (0x80) marks a
   * field optional (1-byte presence prefix); mask with 0x7f for the inner
   * type.
   */
  export const FieldType: {
    readonly I64: 1;
    readonly F64: 2;
    readonly Bool: 3;
    readonly String: 4;
    readonly Bytes: 5;
    readonly PooledStackFrames: 6;
    readonly PooledString: 7;
    readonly StackFrames: 8;
    readonly Varint: 9;
    readonly StringMap: 10;
    readonly U8: 11;
    readonly U16: 12;
    readonly U32: 13;
    readonly DynamicList: 14;
    readonly DynamicMap: 15;
  };

  /**
   * A decoded field value. The wire schema (not compile-time knowledge)
   * decides each field's type, so values are a closed union of everything
   * `decodeFieldValue` can produce:
   * - `null`      absent optional field
   * - `bigint`    I64
   * - `number`    F64 / U8 / U16 / U32, and unresolved pool ids
   * - `boolean`   Bool
   * - `string`    String, Varint (stringified for BigInt safety), resolved
   *               PooledString, StackFrames addresses render as string[]
   * - `number[]`  Bytes
   * - `string[]`  StackFrames / resolved PooledStackFrames (address strings)
   * - `Record<string, string>` StringMap
   * - recursive list/pair forms for DynamicList / DynamicMap
   */
  export type DecodedFieldValue =
    | null
    | bigint
    | number
    | boolean
    | string
    | number[]
    | string[]
    | Record<string, string>
    | DecodedFieldValue[]
    | [DecodedFieldValue, DecodedFieldValue][];

  /** One field declaration inside an on-wire event schema. */
  export interface SchemaField {
    name: string;
    /** FieldType id, possibly with the 0x80 optional bit set. */
    fieldType: number;
  }

  export interface SchemaAnnotation {
    fieldIndex: number;
    key: string;
    value: string;
  }

  /** An event schema accumulated by the decoder (`decoder.schemas`). */
  export interface EventSchema {
    typeId: number;
    name: string;
    hasTimestamp: boolean;
    fields: SchemaField[];
    /** Present once a schema-annotations frame for this schema was decoded. */
    annotations?: SchemaAnnotation[];
    /** field name -> unit string, from `unit` annotations. */
    units?: Record<string, string>;
  }

  export interface SchemaFrame extends EventSchema {
    type: "schema";
  }

  export interface EventFrame {
    type: "event";
    typeId: number;
    /** Schema (event type) name, e.g. "PollStartEvent". */
    name: string;
    /** Decoded field values keyed by schema field name. */
    values: Record<string, DecodedFieldValue>;
    /**
     * Absolute monotonic nanoseconds as a decimal string (BigInt-safe).
     * Only present when the schema has a timestamp.
     */
    timestamp_ns?: string;
  }

  export interface StringPoolFrame {
    type: "string_pool";
    entries: { poolId: number; data: string }[];
  }

  export interface StackPoolFrame {
    type: "stack_pool";
    entries: { poolId: number; addrs: string[] }[];
  }

  export interface SchemaAnnotationsFrame {
    type: "schema_annotations";
    typeId: number;
    annotations: SchemaAnnotation[];
  }

  export type DecodedFrame =
    | SchemaFrame
    | EventFrame
    | StringPoolFrame
    | StackPoolFrame
    | SchemaAnnotationsFrame;

  /** Positional decode state snapshot for streaming rollback. */
  export interface DecoderSnapshot {
    pos: number;
    timestampBaseNs: bigint;
  }

  /**
   * Read-only binary trace decoder. Feed it a whole buffer and pull frames
   * with `nextFrame()` / `decodeAll()`, or drive it incrementally in
   * streaming mode (see `enableStreaming`).
   */
  export class TraceDecoder {
    constructor(buffer: ArrayBuffer | Uint8Array);

    /** Accumulated event schemas, keyed by wire type id. */
    schemas: Map<number, EventSchema>;
    /** Accumulated string pool (pool id -> string). */
    stringPool: Map<number, string>;
    /** Accumulated stack pool (pool id -> address strings). */
    stackPool: Map<number, string[]>;
    /** Trace format version from the header (0 until decodeHeader). */
    version: number;
    /**
     * Streaming mode only: set by `nextFrame()` when it returned null
     * because the buffer ends with an incomplete frame (more bytes coming),
     * as opposed to a real EOF.
     */
    needMoreBytes: boolean;

    /** Current byte offset into the buffer. */
    get position(): number;
    /** Total byte length of the buffer. */
    get byteLength(): number;

    /** Decode the 5-byte `TRC\0` header. False if the magic doesn't match. */
    decodeHeader(): boolean;
    /**
     * Decode the next frame, or null at EOF (or, in streaming mode, when the
     * trailing bytes are an incomplete frame -- see `needMoreBytes`).
     */
    nextFrame(): DecodedFrame | null;
    /** Decode every remaining frame. */
    decodeAll(): DecodedFrame[];

    /** Enable streaming mode (incomplete tail => needMoreBytes, not EOF). */
    enableStreaming(): void;
    /**
     * Replace the underlying buffer (same already-decoded prefix), keeping
     * accumulated schemas/pools/timestamp base. Streaming-parser use.
     */
    setBuffer(buffer: ArrayBuffer | Uint8Array): void;
    /** Snapshot positional state so an incomplete frame can be rolled back. */
    snapshot(): DecoderSnapshot;
    /** Restore a snapshot produced by `snapshot()`. */
    restore(snap: DecoderSnapshot): void;
    /** Reset the read position to buffer start without touching pools. */
    rewindToStart(): void;
  }
}
