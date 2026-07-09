// Hand-written type declarations for the frozen-core file `format.js`.
// See src/types/decode.d.ts for the declaration-form rationale.
// Verified against format.js and call sites in viewer.html/flamegraph.html.

declare module "*/format.js" {
  /**
   * Format a duration in nanoseconds as a human-friendly string
   * ("500ns", "1.5µs", "123.46ms", "5m 12.0s", "2d 4h 30m").
   * Non-finite or negative input renders as "0ns".
   */
  export function formatHumanDuration(ns: number): string;

  /**
   * Format a byte count using binary units ("512 B", "1.50 KiB").
   * Non-finite or negative input renders as "0 B".
   */
  export function formatHumanBytes(bytes: number): string;

  /**
   * Format a field value according to its schema unit annotation
   * ("ns" | "us" | "ms" | "s" | "bytes"). Unknown/missing units fall back
   * to String(value). `value` is genuinely dynamic (schema-driven wire
   * values), hence `unknown`.
   */
  export function formatFieldValue(value: unknown, unit?: string): string;
}
