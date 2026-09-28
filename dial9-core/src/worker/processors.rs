//! Built-in segment processors: gzip compression and disk write-back.

use crate::payload::Payload;
use crate::pipeline::{ProcessError, SegmentData, SegmentProcessor};
use crate::rate_limit::rate_limited;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::time::Duration;

struct RemoveTemporary(PathBuf);

impl Drop for RemoveTemporary {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_file(&self.0)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            rate_limited!(Duration::from_secs(60), {
                tracing::warn!(path = %self.0.display(), ?error, "could not remove temporary trace");
            });
        }
    }
}

fn write_payload_atomically(dest_path: &Path, payload: &Payload) -> std::io::Result<()> {
    write_payload_atomically_with(dest_path, payload, |_| {})
}

// The hook lets tests inspect the file just before its final name becomes visible.
pub(super) fn write_payload_atomically_with(
    dest_path: &Path,
    payload: &Payload,
    before_publish: impl FnOnce(&Path),
) -> std::io::Result<()> {
    use std::io::{BufWriter, Write};

    if let Some(parent) = dest_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut temp_name = dest_path.as_os_str().to_owned();
    temp_name.push(format!(".{}.partial", ulid::Ulid::new()));
    let temporary = PathBuf::from(temp_name);
    let _cleanup = RemoveTemporary(temporary.clone());
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)?;
    let mut writer = BufWriter::new(file);
    for chunk in payload.chunks() {
        writer.write_all(chunk)?;
    }
    writer.flush()?;
    drop(writer);
    before_publish(&temporary);
    std::fs::rename(&temporary, dest_path)
}

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
/// `{original}{extension}` through a temporary file and atomic rename, then
/// the original segment file is removed.
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
                write_payload_atomically(&write_dest, &payload)
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
