//! Trace aggregation building blocks: decode CPU samples and write Parquet.
//!
//! The demand-driven aggregation flow folds raw trace segments into Parquet
//! part-files on demand, decoding samples ([`decode`]) and encoding them
//! ([`parquet_writer`]). [`aggregate`] is the kit of parts (fold one file, read
//! folded part-files, key algebra); [`refine`] is the orchestration layer over
//! it (list → scope → cap → fold → coverage) shared by every demand-driven
//! endpoint.

pub mod aggregate;
pub mod decode;
pub(crate) mod parquet_writer;
pub(crate) mod refine;
