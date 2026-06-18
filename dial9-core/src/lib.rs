/// Thread-local event encoding buffers and the `Encodable` trait.
pub mod buffer;
/// Monotonic/realtime clock readings, the trace time base.
pub mod clock;
/// Central ring buffer of encoded event batches awaiting write.
pub mod collector;
/// Wire-format events emitted by the bus itself.
pub mod format;
/// Writer↔worker filesystem/channel abstraction for trace segments.
#[doc(hidden)]
pub mod fs;
/// Cloneable handle for recording events and controlling telemetry.
pub mod handle;
/// Operational metrics for the core flush path.
pub mod metrics;
/// Segment payload buffer threaded through the worker pipeline.
#[doc(hidden)]
pub mod payload;
/// Cfg-gated concurrency primitives (std / shuttle).
///
/// `pub` so sibling crates share one shuttle shim, but not part of the public
/// API.
#[doc(hidden)]
pub mod primitives;
/// Per-call-site rate limiting for log lines.
#[doc(hidden)]
pub mod rate_limit;
/// Geometric/Poisson sampling primitives (RNG, exponential draws).
#[doc(hidden)]
pub mod sampling;
/// Sealed-segment detection and references for the worker pipeline.
#[doc(hidden)]
pub mod sealed;
/// Runtime-agnostic recording state shared across threads.
pub mod shared_state;
/// `Source` trait: pluggable flush-thread data sources.
pub mod source;
/// Rotating trace-segment writer.
pub mod writer;
