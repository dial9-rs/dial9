//! Samples the application's [metrics.rs](https://docs.rs/metrics) registry
//! into the trace on an interval, so the counters and gauges it already emits
//! sit on the same timeline as runtime telemetry.
//!
//! ```no_run
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! use dial9_utils::metrics_rs::{self, MetricsRsConfig, RecorderMetricsRsExt};
//!
//! let metrics_recorder = metrics_rs::recorder();
//! // metrics.rs takes one global recorder. Applications that already have a
//! // backend compose the two with `metrics_util::layers::FanoutBuilder`.
//! metrics::set_global_recorder(metrics_recorder.clone())?;
//!
//! let writer = dial9_core::buffer::MemoryBuffer::new(1 << 20)?;
//! let dial9_recorder = dial9_core::recorder::recorder(writer)
//!     .with_metrics_rs(metrics_recorder, MetricsRsConfig::default())
//!     .build();
//!
//! // Existing call sites are untouched.
//! metrics::counter!("requests_total", "route" => "/pets").increment(1);
//! # Ok(())
//! # }
//! ```
//!
//! Each metric becomes its own event, named `metricsrs:<metric>{labels}`.
//! `docs/design/metrics-rs-source.md` has the design.

mod capture;
mod encode;
mod source;

pub use source::{
    MetricsRsConfig, MetricsRsConfigBuilder, MetricsRsRecorder, MetricsRsSource,
    RecorderMetricsRsExt, recorder,
};

#[cfg(test)]
mod encode_tests;
#[cfg(test)]
mod source_tests;
