# dial9-viewer

[![Crates.io](https://img.shields.io/crates/v/dial9-viewer.svg)](https://crates.io/crates/dial9-viewer)
![License](https://img.shields.io/crates/l/dial9-viewer.svg)

Library crate backing the [`dial9`](https://crates.io/crates/dial9) CLI. Install
the S3-capable CLI through the `dial9` crate:

```bash
cargo install --locked dial9 --features cli
```

For a local-filesystem-only viewer without S3 or the AWS SDK, install this
crate directly with its default features disabled:

```bash
cargo install --locked dial9-viewer --no-default-features
```

See the [`dial9` README](https://crates.io/crates/dial9) for usage
documentation.

## Simulator mode

Run the complete S3-browser and on-demand aggregation workflow without an S3
bucket or AWS credentials:

```bash
# Sanitized synthetic traces
dial9 serve --simulator --local

# Rebase and replay the bundled demo trace in every virtual segment
dial9 serve --simulator demo --local

# Configure fleet size, segment spacing, data volume, and feature groups
dial9 serve --simulator synthetic \
  --simulator-hosts 8 \
  --simulator-segment-secs 60 \
  --simulator-repetitions 2 \
  --simulator-symbols realistic \
  --simulator-features cpu,scheduling,tasks,spans \
  --local
```

Simulator objects use the production versioned Hive-style layout for segment
object keys and the normal viewer storage API. The catalog is virtual: every
requested time range has deterministic segments, and trace bytes are rendered
only when fetched.
Flamegraph, span, and Tokio-stat rollups are written to a process-local
temporary directory and removed when the server exits. Run `dial9 serve
--help` for all simulator size, duration, and symbol options. Synthetic symbol
names remain anonymous placeholders by default; `--simulator-symbols
realistic` emits deterministic Rust-like names for more representative
flamegraphs.

The S3 Browser reads both the default versioned Hive-style layout for segment
object keys and historical positional keys, so mixed buckets remain browseable.

## Cargo features

S3 storage, bring-your-own AWS credentials, and assume-role support are enabled
by the default `s3` feature. Local-only embedders can omit the AWS SDK
dependency graph:

```toml
dial9-viewer = { version = "0.5", default-features = false }
```

Without `s3`, construct `ViewerConfig` with `local_dir` or `agg_source_dir`.
S3-backed configuration (`bucket`, `agg`, and `agg_output_bucket`) is rejected,
and S3-specific APIs such as `storage::S3Backend`,
`server::AppState::from_bucket`, and the credential types under
`server::credentials` require the `s3` feature.

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
