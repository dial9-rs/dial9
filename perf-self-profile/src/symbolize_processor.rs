use dial9_core::pipeline::{Payload, ProcessError, SegmentData, SegmentProcessor};
use dial9_core::rate_limited;
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

// ---------------------------------------------------------------------------
// SymbolizeProcessor — resolves stack frame addresses to symbol names
// ---------------------------------------------------------------------------

/// Resolves stack-frame addresses in the segment to symbol names using
/// the current process's `/proc/self/maps`.
///
/// Owns a long-lived
/// [`OfflineSymbolizer`](crate::offline_symbolize::OfflineSymbolizer)
/// running on a dedicated thread, so blazesym's per-ELF DWARF cache
/// stays warm across segments. Without this, every segment paid the
/// full ELF parse cost (hundreds of ms — see #462).
pub struct SymbolizeProcessor {
    symbolizer: std::sync::Arc<crate::offline_symbolize::OfflineSymbolizer>,
}

impl SymbolizeProcessor {
    pub fn new() -> Self {
        Self {
            symbolizer: std::sync::Arc::new(crate::offline_symbolize::OfflineSymbolizer::new()),
        }
    }
}

impl Default for SymbolizeProcessor {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for SymbolizeProcessor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SymbolizeProcessor").finish_non_exhaustive()
    }
}

impl SegmentProcessor for SymbolizeProcessor {
    fn name(&self) -> &'static str {
        "Symbolize"
    }

    fn process(
        &mut self,
        mut data: SegmentData,
    ) -> Pin<Box<dyn Future<Output = Result<SegmentData, ProcessError>> + Send + '_>> {
        let symbolizer = self.symbolizer.clone();
        Box::pin(async move {
            // Skip already-compressed segments (e.g. leftover from a previous run).
            if data.payload().starts_with(&[0x1f, 0x8b]) {
                tracing::debug!(target: "dial9_worker", "segment is gzip-compressed, skipping symbolization");
                return Ok(data);
            }
            // The symbolize FFI reads `&[u8]`, so we materialize a single
            // contiguous `Bytes`. When there's only one chunk this is a
            // zero-copy `Bytes::clone`-equivalent; the `BytesMut` concat
            // path runs only on already-segmented input (rare).
            let input = data.take_payload().into_bytes();
            // Hand off to a blocking thread because `OfflineSymbolizer::symbolize`
            // is itself a blocking call (it sends to its dedicated symbolizer
            // thread and waits for the response).
            let result = tokio::task::spawn_blocking(move || {
                let maps = crate::read_proc_maps();
                let output = symbolizer.symbolize_bytes(input.clone(), &maps)?;
                // Hand back the original bytes plus the symbol output as two
                // chunks — no copy of `input`.
                let mut combined = Payload::new();
                combined.push(input);
                combined.push(bytes::Bytes::from(output));
                Ok::<_, std::io::Error>(combined)
            })
            .await;
            match result {
                Ok(Ok(payload)) => {
                    data.set_payload(payload);
                    Ok(data)
                }
                Ok(Err(e)) => {
                    rate_limited!(Duration::from_secs(60), {
                        tracing::warn!(target: "dial9_worker", error = %e, "symbolization failed, preserving original bytes");
                    });
                    Err(ProcessError::io(data, e))
                }
                Err(e) => Err(ProcessError::io(data, std::io::Error::other(e))),
            }
        })
    }
}
