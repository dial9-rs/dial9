/// Monotonic/realtime clock readings, the trace time base.
pub mod clock;
/// Wire-format events emitted by the bus itself.
pub mod format;
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
