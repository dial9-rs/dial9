// The wire schema arrives before any event of its type and declares each
// field's name and type, so the columns type themselves from it: a Varint field
// becomes a Float64 column, a pooled string becomes an interned index, and only
// the genuinely dynamic types (maps, lists, byte blobs) stay boxed.
//
// Rows keep arrival order in flat `ts`/`nameIdx`/`schemaIdx` columns and carry
// a `slotIdx` into their schema's own field columns, so events of unrelated
// schemas interleave without a column per (schema, field) on every row.

import type { DecodedFieldValue } from "../../../decode.js";
import type {
  CustomTraceEvent,
  SingleEventSpan,
} from "../../../trace_parser.js";

/** Wire field types (dial9-trace-format/js/decode.js FieldType). */
const F = {
  I64: 1, F64: 2, Bool: 3, String: 4, Bytes: 5, PooledStackFrames: 6,
  PooledString: 7, StackFrames: 8, Varint: 9, StringMap: 10, U8: 11,
  U16: 12, U32: 13, DynamicList: 14, DynamicMap: 15,
} as const;

/** High bit of a field type marks it optional; the value may decode as null. */
const OPTIONAL_BIT = 0x80;

const INITIAL = 1 << 10;

/** A wire schema, as the decoder builds it. */
export interface WireSchema {
  name: string;
  fields: readonly { name: string; fieldType: number }[];
  units?: Record<string, string> | null;
  fieldKinds?: Record<string, string> | null;
}

/**
 * How one field is stored. `num` covers every fixed-width numeric type and
 * `bool` is the same column read back as a boolean; `text` interns into the
 * store's string pool; `exact` (Varint, read back as a decimal string) and
 * `i64` (read back as a BigInt) are numeric columns with a side table for
 * values a double cannot hold; `boxed` keeps the decoded value as-is.
 */
type ColumnKind = "num" | "bool" | "text" | "exact" | "i64" | "boxed";

interface FieldColumn {
  name: string;
  kind: ColumnKind;
  /** Numeric/interned storage; unused for `boxed`. */
  nums: Float64Array | Uint8Array | Uint16Array | Uint32Array | Int32Array;
  /** Values a double cannot represent, by slot. Only for `exact` and `i64`. */
  exact?: Map<number, string | bigint>;
  /** Decoded values, by slot. Only for `boxed`. */
  boxed?: DecodedFieldValue[];
  /** 1 when the field was present. Only allocated for optional fields. */
  present?: Uint8Array;
}

interface SchemaGroup {
  schema: WireSchema;
  columns: FieldColumn[];
  /** Rows written into this group so far. */
  n: number;
  cap: number;
}

/** Column storage for one wire field type. */
function columnKindFor(fieldType: number): ColumnKind {
  switch (fieldType & 0x7f) {
    case F.Bool:
      return "bool";
    case F.U8:
    case F.U16:
    case F.U32:
    case F.F64:
      return "num";
    case F.String:
    case F.PooledString:
    case F.PooledStackFrames:
      return "text";
    case F.Varint:
      return "exact";
    case F.I64:
      return "i64";
    default:
      return "boxed";
  }
}

function numArrayFor(fieldType: number, cap: number): FieldColumn["nums"] {
  switch (fieldType & 0x7f) {
    case F.Bool:
    case F.U8:
      return new Uint8Array(cap);
    case F.U16:
      return new Uint16Array(cap);
    case F.U32:
      return new Uint32Array(cap);
    default:
      return new Float64Array(cap);
  }
}

function grow(old: FieldColumn["nums"], cap: number): FieldColumn["nums"] {
  const Ctor = old.constructor as new (n: number) => FieldColumn["nums"];
  const next = new Ctor(cap);
  (next as unknown as { set(a: ArrayLike<number>): void }).set(old);
  return next;
}

export class ColumnarCustomEvents {
  private ts = new Float64Array(INITIAL);
  private nameIdx = new Int32Array(INITIAL);
  private schemaIdx = new Int32Array(INITIAL);
  private slotIdx = new Int32Array(INITIAL);
  private n = 0;
  private cap = INITIAL;

  private names: string[] = [];
  private nameIntern = new Map<string, number>();
  private strings: string[] = [];
  private stringIntern = new Map<string, number>();

  private groups: SchemaGroup[] = [];
  private groupOf = new Map<WireSchema, number>();

  /** Rows whose schema annotations projected a completed span. Rare. */
  private spans = new Map<number, SingleEventSpan>();

  get length(): number {
    return this.n;
  }

  private internName(s: string): number {
    let i = this.nameIntern.get(s);
    if (i === undefined) {
      i = this.names.length;
      this.names.push(s);
      this.nameIntern.set(s, i);
    }
    return i;
  }

  private internString(s: string): number {
    let i = this.stringIntern.get(s);
    if (i === undefined) {
      i = this.strings.length;
      this.strings.push(s);
      this.stringIntern.set(s, i);
    }
    return i;
  }

  private groupFor(schema: WireSchema): SchemaGroup {
    const known = this.groupOf.get(schema);
    if (known !== undefined) return this.groups[known]!;
    const columns: FieldColumn[] = schema.fields.map((f) => {
      const kind = columnKindFor(f.fieldType);
      const col: FieldColumn = {
        name: f.name,
        kind,
        nums: kind === "text" ? new Int32Array(INITIAL) : numArrayFor(f.fieldType, INITIAL),
      };
      if (kind === "exact" || kind === "i64") col.exact = new Map();
      if (kind === "boxed") col.boxed = [];
      if (f.fieldType & OPTIONAL_BIT) col.present = new Uint8Array(INITIAL);
      return col;
    });
    const group: SchemaGroup = { schema, columns, n: 0, cap: INITIAL };
    this.groupOf.set(schema, this.groups.length);
    this.groups.push(group);
    return group;
  }

  private growRows(): void {
    const c = this.cap * 2;
    const g = <T extends { set(a: ArrayLike<number>): void }>(
      old: ArrayLike<number>,
      Ctor: new (n: number) => T,
    ): T => {
      const next = new Ctor(c);
      next.set(old as ArrayLike<number>);
      return next;
    };
    this.ts = g(this.ts, Float64Array);
    this.nameIdx = g(this.nameIdx, Int32Array);
    this.schemaIdx = g(this.schemaIdx, Int32Array);
    this.slotIdx = g(this.slotIdx, Int32Array);
    this.cap = c;
  }

  private growGroup(group: SchemaGroup): void {
    const c = group.cap * 2;
    for (const col of group.columns) {
      col.nums = grow(col.nums, c);
      if (col.present) {
        const p = new Uint8Array(c);
        p.set(col.present);
        col.present = p;
      }
    }
    group.cap = c;
  }

  /**
   * SINK: one non-span custom event, with the schema the decoder resolved it
   * against. `fields` carries every declared field by name, null for an absent
   * optional.
   */
  pushCustom(
    name: string,
    timestamp: number,
    fields: Record<string, DecodedFieldValue>,
    schema: WireSchema | null | undefined,
    singleEventSpan: SingleEventSpan | null,
  ): void {
    if (this.n === this.cap) this.growRows();
    const row = this.n++;
    this.ts[row] = timestamp;
    this.nameIdx[row] = this.internName(name);
    if (singleEventSpan != null) this.spans.set(row, singleEventSpan);

    if (!schema) {
      // No schema on the wire for this event: nothing to type the columns
      // from, so the row carries only its name and timestamp.
      this.schemaIdx[row] = -1;
      this.slotIdx[row] = -1;
      return;
    }
    const group = this.groupFor(schema);
    if (group.n === group.cap) this.growGroup(group);
    const slot = group.n++;
    this.schemaIdx[row] = this.groupOf.get(schema)!;
    this.slotIdx[row] = slot;

    for (let f = 0; f < group.columns.length; f++) {
      const col = group.columns[f]!;
      const v = fields[col.name];
      if (v == null) {
        if (col.present) col.present[slot] = 0;
        else if (col.kind === "boxed") col.boxed![slot] = null;
        continue;
      }
      if (col.present) col.present[slot] = 1;
      switch (col.kind) {
        case "num":
          col.nums[slot] = v as number;
          break;
        case "bool":
          col.nums[slot] = v ? 1 : 0;
          break;
        case "text":
          col.nums[slot] = this.internString(String(v));
          break;
        case "exact": {
          // Varints decode to u64 decimal strings, which a double only holds
          // below 2^53. Keep the original text for the rest rather than
          // rounding it silently.
          const text = String(v);
          const num = Number(text);
          col.nums[slot] = num;
          if (String(num) !== text) col.exact!.set(slot, text);
          break;
        }
        case "i64": {
          const big = v as bigint;
          const num = Number(big);
          col.nums[slot] = num;
          if (BigInt(num) !== big) col.exact!.set(slot, big);
          break;
        }
        case "boxed":
          col.boxed![slot] = v;
          break;
      }
    }
  }

  /** The decoded value of `col` at `slot`, in the shape the fat event had. */
  private valueAt(col: FieldColumn, slot: number): DecodedFieldValue {
    if (col.present && col.present[slot] === 0) return null;
    switch (col.kind) {
      case "num":
        return col.nums[slot]!;
      case "bool":
        return col.nums[slot] === 1;
      case "text":
        return this.strings[col.nums[slot]!]!;
      case "exact": {
        const exact = col.exact!.get(slot);
        return exact !== undefined ? exact : String(col.nums[slot]!);
      }
      case "i64": {
        const exact = col.exact!.get(slot);
        return exact !== undefined ? exact : BigInt(col.nums[slot]!);
      }
      case "boxed":
        return col.boxed![slot] ?? null;
    }
  }

  /** Field name of row `i`'s schema, without materializing the row. */
  nameAt(i: number): string {
    return this.names[this.nameIdx[i]!]!;
  }

  tsAt(i: number): number {
    return this.ts[i]!;
  }

  /** Every distinct event name in the store. */
  eventNames(): string[] {
    return [...this.names];
  }

  /** Materialize row `i` as the fat event shape consumers expect. */
  at(i: number): CustomTraceEvent | undefined {
    if (i < 0 || i >= this.n) return undefined;
    const si = this.schemaIdx[i]!;
    const fields: Record<string, DecodedFieldValue> = {};
    let units: Record<string, string> | null = null;
    let fieldKinds: Record<string, string> | null = null;
    if (si >= 0) {
      const group = this.groups[si]!;
      const slot = this.slotIdx[i]!;
      // The decoder writes every declared field, null for an absent optional,
      // so the key is always there.
      for (const col of group.columns) fields[col.name] = this.valueAt(col, slot);
      units = group.schema.units ?? null;
      fieldKinds = group.schema.fieldKinds ?? null;
    }
    const out: CustomTraceEvent = {
      name: this.names[this.nameIdx[i]!]!,
      timestamp: this.ts[i]!,
      fields,
      units,
      fieldKinds,
      singleEventSpan: this.spans.get(i) ?? null,
    };
    return out;
  }

  *[Symbol.iterator](): IterableIterator<CustomTraceEvent> {
    for (let i = 0; i < this.n; i++) yield this.at(i)!;
  }

  find(
    pred: (e: CustomTraceEvent, i: number) => boolean,
  ): CustomTraceEvent | undefined {
    for (let i = 0; i < this.n; i++) {
      const e = this.at(i)!;
      if (pred(e, i)) return e;
    }
    return undefined;
  }

  /** Array-compatible read for the consumers that still scan the fat shape. */
  filter(
    pred: (e: CustomTraceEvent, i: number) => boolean,
  ): CustomTraceEvent[] {
    const out: CustomTraceEvent[] = [];
    for (let i = 0; i < this.n; i++) {
      const e = this.at(i)!;
      if (pred(e, i)) out.push(e);
    }
    return out;
  }
}
