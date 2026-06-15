/// Cfg-gated concurrency primitives (std / shuttle).
pub mod primitives;
/// Per-call-site rate limiting for log lines.
pub mod rate_limit;
/// Geometric/Poisson sampling primitives (RNG, exponential draws).
pub mod sampling;
