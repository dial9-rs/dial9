//! Trace aggregation building blocks: decode CPU samples and write Parquet.
//!
//! The demand-driven aggregation flow ([`aggregate`]) folds raw trace segments
//! into Parquet part-files on demand, decoding samples ([`decode`]) and encoding
//! them ([`parquet_writer`]). These modules are the core of the
//! `/api/flamegraph` refinement loop.

pub mod aggregate;
pub mod decode;
pub mod parquet_writer;
