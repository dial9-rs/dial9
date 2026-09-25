use dial9_core::pipeline::{Payload, ProcessError, SegmentData, SegmentProcessor};
use dial9_core::rate_limited;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::sync::Notify;

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
    symbolizer: Arc<crate::offline_symbolize::OfflineSymbolizer>,
    busy: Arc<AtomicBool>,
    available: Arc<Notify>,
}

impl SymbolizeProcessor {
    pub fn new() -> Self {
        Self {
            symbolizer: Arc::new(crate::offline_symbolize::OfflineSymbolizer::new()),
            busy: Arc::new(AtomicBool::new(false)),
            available: Arc::new(Notify::new()),
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

    fn wait_until_live(&mut self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(async move {
            loop {
                let notified = self.available.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                if !self.busy.load(Ordering::Acquire) {
                    return;
                }
                notified.await;
            }
        })
    }

    fn process(
        &mut self,
        mut data: SegmentData,
    ) -> Pin<Box<dyn Future<Output = Result<SegmentData, ProcessError>> + Send + '_>> {
        // Skip already-compressed segments (e.g. leftover from a previous run).
        if data.payload().starts_with(&[0x1f, 0x8b]) {
            tracing::debug!(target: "dial9_worker", "segment is gzip-compressed, skipping symbolization");
            return Box::pin(async move { Ok(data) });
        }

        let symbolizer = self.symbolizer.clone();
        let Some(busy) = BusyGuard::try_acquire(self.busy.clone(), self.available.clone()) else {
            return Box::pin(async move {
                Err(ProcessError::io(
                    data,
                    std::io::Error::other("symbolization is already in progress"),
                ))
            });
        };
        Box::pin(async move {
            // The symbolize FFI reads `&[u8]`, so we materialize a single
            // contiguous `Bytes`. When there's only one chunk this is a
            // zero-copy `Bytes::clone`-equivalent; the `BytesMut` concat
            // path runs only on already-segmented input (rare).
            let input = data.take_payload().into_bytes();
            // Hand off to a blocking thread because `OfflineSymbolizer::symbolize`
            // is itself a blocking call (it sends to its dedicated symbolizer
            // thread and waits for the response).
            let result = tokio::task::spawn_blocking(move || {
                let _busy = busy;
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

struct BusyGuard {
    busy: Arc<AtomicBool>,
    available: Arc<Notify>,
}

impl BusyGuard {
    fn try_acquire(busy: Arc<AtomicBool>, available: Arc<Notify>) -> Option<Self> {
        busy.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .ok()
            .map(|_| Self { busy, available })
    }
}

impl Drop for BusyGuard {
    fn drop(&mut self) {
        self.busy.store(false, Ordering::Release);
        self.available.notify_one();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn symbolizer_waits_while_busy() {
        let mut processor = SymbolizeProcessor::new();
        processor.wait_until_live().await;

        let guard =
            BusyGuard::try_acquire(processor.busy.clone(), processor.available.clone()).unwrap();
        {
            let wait = processor.wait_until_live();
            tokio::pin!(wait);
            assert!(
                tokio::time::timeout(Duration::ZERO, wait.as_mut())
                    .await
                    .is_err()
            );
            drop(guard);
            wait.as_mut().await;
        }
        assert!(
            BusyGuard::try_acquire(processor.busy.clone(), processor.available.clone()).is_some()
        );
    }
}
