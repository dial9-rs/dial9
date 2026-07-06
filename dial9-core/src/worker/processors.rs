//! Built-in segment processors: gzip compression and disk write-back.

use crate::payload::Payload;
use crate::pipeline::{ProcessError, SegmentData, SegmentProcessor};
use crate::rate_limit::rate_limited;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::time::Duration;

/// Gzips the segment payload in-memory. Sets the `content_encoding` and
/// `write_back_extension` metadata keys so downstream stages know the
/// payload is gzipped. Already-gzipped segments (detected by magic bytes)
/// pass through unchanged.
#[derive(Debug, Default)]
pub struct GzipCompressor;

impl SegmentProcessor for GzipCompressor {
    fn name(&self) -> &'static str {
        "Gzip"
    }

    fn process(
        &mut self,
        mut data: SegmentData,
    ) -> Pin<Box<dyn Future<Output = Result<SegmentData, ProcessError>> + Send + '_>> {
        Box::pin(async move {
            // Skip already-compressed segments to avoid double-gzip.
            if data.payload().starts_with(&[0x1f, 0x8b]) {
                data.metadata_mut()
                    .insert("content_encoding".into(), "gzip".into());
                data.metadata_mut()
                    .insert("write_back_extension".into(), ".gz".into());
                return Ok(data);
            }
            let raw = data.take_payload();
            let compressed = tokio::task::spawn_blocking(move || {
                use flate2::write::GzEncoder;
                use std::io::Write;
                let mut encoder = GzEncoder::new(Vec::new(), flate2::Compression::fast());
                for chunk in raw.chunks() {
                    encoder.write_all(chunk)?;
                }
                encoder.finish()
            })
            .await;
            match compressed {
                Ok(Ok(bytes)) => {
                    data.set_compressed_size(bytes.len() as u64);
                    data.set_payload(Payload::from_vec(bytes));
                    data.metadata_mut()
                        .insert("content_encoding".into(), "gzip".into());
                    data.metadata_mut()
                        .insert("write_back_extension".into(), ".gz".into());
                    Ok(data)
                }
                Ok(Err(e)) => Err(ProcessError::io(data, e)),
                Err(e) => Err(ProcessError::io(data, std::io::Error::other(e))),
            }
        })
    }
}

/// Writes the current payload bytes back to disk. If a
/// `write_back_extension` metadata key is present, the bytes are written to
/// `{original}{extension}` and the original segment file is removed.
/// When `dir` is set, the file is written to that directory instead of
/// alongside the original.
#[derive(Debug, Default)]
pub struct WriteBackProcessor {
    dir: Option<PathBuf>,
}

impl WriteBackProcessor {
    /// Write to `dir` instead of alongside the original segment.
    pub fn to_dir(dir: PathBuf) -> Self {
        Self { dir: Some(dir) }
    }
}

impl SegmentProcessor for WriteBackProcessor {
    fn name(&self) -> &'static str {
        "WriteBack"
    }

    fn process(
        &mut self,
        data: SegmentData,
    ) -> Pin<Box<dyn Future<Output = Result<SegmentData, ProcessError>> + Send + '_>> {
        let output_dir = self.dir.clone();
        Box::pin(async move {
            let original_path = match data.segment().disk_path() {
                Some(p) => p.to_owned(),
                None => {
                    return Err(ProcessError::io(
                        data,
                        std::io::Error::other(
                            "WriteBackProcessor requires a disk-backed segment; \
                             memory-backed segments must not use write_back()",
                        ),
                    ));
                }
            };
            let base_path = match &output_dir {
                Some(dir) => dir.join(original_path.file_name().unwrap_or_default()),
                None => original_path.clone(),
            };
            let dest_path = match data.metadata().get("write_back_extension") {
                Some(ext) => {
                    let mut p = base_path.as_os_str().to_owned();
                    p.push(ext);
                    std::path::PathBuf::from(p)
                }
                None => base_path,
            };
            let payload = data.payload().clone();
            let write_dest = dest_path.clone();
            let result = tokio::task::spawn_blocking(move || {
                use std::io::{BufWriter, Write};
                if let Some(parent) = write_dest.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                let mut f = BufWriter::new(std::fs::File::create(&write_dest)?);
                for chunk in payload.chunks() {
                    f.write_all(chunk)?;
                }
                f.flush()
            })
            .await;
            match result {
                Ok(Ok(())) => {
                    if dest_path != original_path {
                        // Remove the original .bin now that the output exists elsewhere.
                        // If the writer already evicted it, clean up the dest
                        // file we just wrote so it doesn't leak on disk.
                        match std::fs::remove_file(&original_path) {
                            Ok(()) => {}
                            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                                let _ = std::fs::remove_file(&dest_path);
                            }
                            Err(e) => {
                                rate_limited!(Duration::from_secs(60), {
                                    tracing::warn!(
                                        "failed to remove original segment {}: {e}",
                                        original_path.display()
                                    );
                                });
                            }
                        }
                    }
                    Ok(data)
                }
                Ok(Err(e)) => Err(ProcessError::io(data, e)),
                Err(e) => Err(ProcessError::io(data, std::io::Error::other(e))),
            }
        })
    }
}
