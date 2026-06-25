/// Declares an item `pub` under the `test-util` feature and `pub(crate)`
/// otherwise. Lets sibling-crate tests reach internals (batches, the collector,
/// raw encode/drain helpers) without putting them on the production public API.
macro_rules! test_util_pub {
    ($(#[$attr:meta])* $kw:ident $name:ident $($rest:tt)*) => {
        #[cfg(feature = "test-util")]
        $(#[$attr])* pub $kw $name $($rest)*
        // Without the feature these are crate-internal and only the test-util
        // consumers reference them, so dead-code analysis would flag them.
        #[cfg(not(feature = "test-util"))]
        $(#[$attr])* #[allow(dead_code)] pub(crate) $kw $name $($rest)*
    };
}
pub(crate) use test_util_pub;

/// Shared boot identifier (`{4-alpha}-{pid}`) for segment namespacing + S3 keys.
#[doc(hidden)]
pub mod boot_id;
/// Thread-local event encoding buffers and the `Encodable` trait.
pub mod buffer;
/// Monotonic/realtime clock readings, the trace time base.
pub mod clock;
/// Central ring buffer of encoded event batches awaiting write.
pub mod collector;
/// On-demand pipeline runs: trigger, request channel, and dump receipts.
#[cfg(feature = "pipeline")]
pub mod dump;
/// Flush-thread loop. Driven by `CoreSession`; not public API.
pub(crate) mod flush_loop;
/// Wire-format events emitted by the bus itself.
pub mod format;
/// Writer↔worker filesystem/channel abstraction for trace segments.
#[doc(hidden)]
pub mod fs;
/// Cloneable handle for recording events and controlling telemetry.
pub mod handle;
/// Operational metrics for the core flush path.
pub(crate) mod metrics;
/// Segment payload buffer. `Payload` is part of the public API via [`pipeline`].
#[cfg(feature = "pipeline")]
pub(crate) mod payload;
/// `SegmentProcessor` trait and the data threaded through the pipeline.
#[cfg(feature = "pipeline")]
pub mod pipeline;
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
/// Sealed-segment detection. The segment types are public via [`pipeline`].
pub(crate) mod sealed;
/// Runtime-agnostic recording session (handle + flush thread lifecycle).
pub mod session;
/// Runtime-agnostic recording state shared across threads.
pub mod shared_state;
/// `Source` trait: pluggable flush-thread data sources.
pub mod source;
/// Segment-processing worker: runs a `SegmentProcessor` pipeline over sealed segments.
#[cfg(feature = "pipeline")]
pub mod worker;
/// Rotating trace-segment writer.
pub mod writer;
