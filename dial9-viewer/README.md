# dial9-viewer

[![Crates.io](https://img.shields.io/crates/v/dial9-viewer.svg)](https://crates.io/crates/dial9-viewer)
![License](https://img.shields.io/crates/l/dial9-viewer.svg)

Library crate backing the [`dial9`](https://crates.io/crates/dial9) CLI. Install and use via the `dial9` crate:

```bash
cargo install --locked dial9
```

See the [`dial9` README](https://crates.io/crates/dial9) for usage documentation.

## `trace-shape` — Trace Structural Fingerprints

The `trace-shape` subcommand extracts and generates sanitized structural
fingerprints ("shapes") from dial9 traces. Shapes preserve the operational
characteristics of a trace—event types, timing distributions, cardinality,
field schemas—with best-effort removal of labels, identifiers, payloads, and
exact timestamps. This is not an anonymization or security boundary.

### Usage

```bash
# Sanitize directly into a synthetic trace (preferred for large traces)
dial9 trace-shape synthesize /tmp/traces/trace.bin synthetic.bin

# Repeat the in-memory template 5 times without writing shape JSON
dial9 trace-shape synthesize /tmp/traces/trace.bin synthetic.bin --repeat 5

# Extract a portable shape from a trace file (accepts gzip input)
dial9 trace-shape extract /tmp/traces/trace.bin shape.json

# Generate a synthetic trace from a previously extracted shape
dial9 trace-shape generate shape.json synthetic.bin
```

`trace-shape synthesize` builds the same sanitized shape in memory and writes
its synthetic trace directly. It avoids the verbose per-event JSON intermediate
and its input-size limit, while preserving the same privacy transformations,
correlations, validation, and repeat behavior as `extract` followed by
`generate`.

### What shapes preserve

- Event types, field schemas, and field types
- Relative timing and event ordering (quantized to 10 µs)
- Worker and task cardinality
- Value magnitudes and distributions (quantized)
- Byte payload lengths, stack depths, and dynamic-container cardinalities
- String values replaced with fixed deterministic placeholders (source lengths are not retained)
- Correlated identity namespaces (task, span, thread, address)
- Built-in schema/field names needed by the viewer

### What shapes remove

- Source absolute timestamps (monotonic and realtime)
- String contents (replaced with fixed-length deterministic placeholders, e.g. `s_0001`)
- Custom schema and field names (replaced with anonymous names)
- Stack addresses (replaced with synthetic deterministic addresses)
- Byte payloads (zeroed, length preserved)
- Arbitrary annotation text (only allowlisted unit annotations — `ns`, `us`,
  `ms`, `s`, `bytes`, `count` — are retained)

### Fixed-width normalization

The trace format's `U8`, `U16`, and `U32` wire types are normalized to varints
in shape files. The Encoder dispatches on `FieldValue` variants (not schema
`FieldType`), so decoded `Varint` values cannot be re-encoded as fixed-width.
Hand-authored shapes using fixed-width tags are rejected at validation time.

### Privacy caveat

Shape extraction applies deterministic transformations to remove string
contents, byte payloads, exact timestamps, custom names, and stack addresses.
However, exact booleans, small quantized integers in built-in schemas, and
already-round floats may survive transformation. This is **not an anonymization
or security boundary**. Shapes intentionally **retain sensitive operational
structure** including relative timing, event ordering, cardinality, byte payload
sizes, stack depths, value magnitude distributions, and inter-event
correlations. This operational structure may reveal performance characteristics,
traffic patterns, or architectural details. Synthetic traces generated from
shapes should be treated as confidential operational data and shared only with
the same caution as production metrics.
