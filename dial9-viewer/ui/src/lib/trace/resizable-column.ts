// Columns grow by resize, not by allocate-and-copy: a resizable ArrayBuffer
// reserves address space once and commits pages as it grows, so no second
// buffer is ever resident. Worth 1.1 GB of peak on a 30M-event parse (#648).
//
// Views are length-tracking (created with no explicit length), so they follow
// the buffer across a resize and never need rebinding.

/** ES2024 resizable ArrayBuffer, not in the ES2022 lib this project targets. */
interface ResizableArrayBuffer extends ArrayBuffer {
  resize(byteLength: number): void;
}
type ResizableArrayBufferCtor = new (
  byteLength: number,
  options: { maxByteLength: number },
) => ResizableArrayBuffer;

/** Any typed-array column: a view with a known element width. */
type Column = ArrayBufferView & { readonly BYTES_PER_ELEMENT: number };

interface ColumnCtor<T> {
  new (buffer: ArrayBuffer): T;
  new (length: number): T;
  readonly BYTES_PER_ELEMENT: number;
}

/** Elements a column can reach before it falls back to copy growth. The
 *  reservation is virtual, but it is still address space, so it is bounded. */
export const MAX_RESIZABLE_ELEMENTS = 1 << 25; // ~33.5M

const supported = ((): boolean => {
  try {
    const b = new (ArrayBuffer as unknown as ResizableArrayBufferCtor)(8, {
      maxByteLength: 16,
    });
    b.resize(16);
    return b.byteLength === 16;
  } catch {
    return false;
  }
})();

/**
 * Allocate one column of `cap` elements, resizable up to
 * {@link MAX_RESIZABLE_ELEMENTS} where the runtime supports it. A capacity
 * already past the ceiling allocates fixed and grows by copy.
 */
export function allocColumn<T>(Ctor: ColumnCtor<T>, cap: number): T {
  if (!supported || cap > MAX_RESIZABLE_ELEMENTS) return new Ctor(cap);
  const buf = new (ArrayBuffer as unknown as ResizableArrayBufferCtor)(
    cap * Ctor.BYTES_PER_ELEMENT,
    { maxByteLength: MAX_RESIZABLE_ELEMENTS * Ctor.BYTES_PER_ELEMENT },
  );
  return new Ctor(buf);
}

/**
 * Resize every column to `cap` elements in place. Returns false when any of
 * them is not resizable (unsupported runtime, or `cap` past the ceiling),
 * leaving all of them untouched for the caller to grow by copy.
 */
export function resizeColumns(cols: readonly Column[], cap: number): boolean {
  if (!supported || cap > MAX_RESIZABLE_ELEMENTS) return false;
  for (const c of cols) {
    if (!("resize" in c.buffer)) return false;
  }
  for (const c of cols) {
    (c.buffer as ResizableArrayBuffer).resize(cap * c.BYTES_PER_ELEMENT);
  }
  return true;
}
