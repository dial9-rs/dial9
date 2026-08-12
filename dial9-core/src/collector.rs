use crate::primitives::BoundedQueue;
use crate::primitives::sync::atomic::{AtomicUsize, Ordering};

/// Maximum number of encoded batches that can be buffered.
/// Beyond this, the oldest batch is evicted — the queue acts as a ring buffer
/// so the most recent data is always preserved.
/// With ~1MB batches, 1024 slots is generous; the flush thread drains every
/// 5ms so in practice the queue rarely has more than a handful of entries.
const DEFAULT_CAPACITY: usize = 1024;

crate::test_util_pub! {
/// A batch of encoded trace events ready for writing.
#[derive(Debug)]
#[non_exhaustive]
struct Batch {
    pub(crate) encoded_bytes: Vec<u8>,
    pub(crate) event_count: u64,
}
}

impl Batch {
    crate::test_util_pub! {
    /// Create a new batch from encoded bytes and an event count.
    fn new(encoded_bytes: Vec<u8>, event_count: u64) -> Self {
        Self {
            encoded_bytes,
            event_count,
        }
    }
    }

    crate::test_util_pub! {
    /// The encoded trace bytes for this batch.
    fn encoded_bytes(&self) -> &[u8] {
        &self.encoded_bytes
    }
    }

    /// Number of events in this batch.
    pub(crate) fn event_count(&self) -> u64 {
        self.event_count
    }

    /// Whether this batch contains no events.
    pub(crate) fn is_empty(&self) -> bool {
        self.event_count == 0
    }

    /// Consume the batch, returning the encoded bytes without copying.
    pub(crate) fn into_encoded_bytes(self) -> Vec<u8> {
        self.encoded_bytes
    }
}

/// Ring buffer of encoded batches awaiting write by the flush thread.
pub(crate) struct CentralCollector {
    queue: BoundedQueue<Batch>,
    dropped_batches: AtomicUsize,
}

impl Default for CentralCollector {
    fn default() -> Self {
        Self::new()
    }
}

impl CentralCollector {
    pub(crate) fn new() -> Self {
        Self::with_capacity(DEFAULT_CAPACITY)
    }

    pub(crate) fn with_capacity(capacity: usize) -> Self {
        Self {
            queue: BoundedQueue::new(capacity),
            dropped_batches: AtomicUsize::new(0),
        }
    }

    pub(crate) fn accept_flush(&self, batch: Batch) {
        if let Some(_evicted) = self.queue.force_push(batch) {
            self.dropped_batches.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub(crate) fn next(&self) -> Option<Batch> {
        self.queue.pop()
    }

    /// Returns the number of batches dropped since the last call.
    pub(crate) fn take_dropped_batches(&self) -> usize {
        self.dropped_batches.swap(0, Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_batch(size: usize) -> Batch {
        Batch {
            encoded_bytes: vec![0u8; size],
            event_count: 1,
        }
    }

    #[test]
    fn test_drain_clears_buffers() {
        let collector = CentralCollector::new();
        collector.accept_flush(dummy_batch(10));
        assert!(collector.next().is_some());
        assert!(collector.next().is_none());
    }

    fn drain(collector: &CentralCollector) -> Vec<Vec<u8>> {
        let mut out = vec![];
        while let Some(batch) = collector.next() {
            out.push(batch.encoded_bytes);
        }
        out
    }

    #[test]
    fn test_bounded_evicts_oldest_when_full() {
        let collector = CentralCollector::with_capacity(2);
        collector.accept_flush(dummy_batch(1)); // oldest — will be evicted
        collector.accept_flush(dummy_batch(2));
        collector.accept_flush(dummy_batch(3)); // evicts first
        assert_eq!(collector.take_dropped_batches(), 1);
        let drained = drain(&collector);
        assert_eq!(drained.len(), 2);
        // oldest (len=1) was evicted; remaining are len=2 and len=3
        assert_eq!(drained[0].len(), 2);
        assert_eq!(drained[1].len(), 3);
    }
}

#[cfg(all(test, shuttle))]
mod shuttle_tests {
    use super::*;
    use crate::primitives::sync::Arc;

    const PUSHERS: usize = 3;
    const BATCHES_PER_PUSHER: u64 = 3;
    /// Tight enough that the writer threads outrun the popper and force
    /// `BoundedQueue::force_push`'s evict-oldest path under contention.
    const CAPACITY: usize = 2;

    crate::shuttle_test! {
        num_iters = 5_000, depth = 3;
        // Multiple pusher threads race a popper thread against a tight capacity.
        // Every pushed batch must be either popped or evicted exactly once.
        fn shuttle_collector_eviction_accounting() {
            let collector = Arc::new(CentralCollector::with_capacity(CAPACITY));

            let pushers: Vec<_> = (0..PUSHERS)
                .map(|p| {
                    let collector = collector.clone();
                    crate::primitives::thread::spawn(move || {
                        for i in 0..BATCHES_PER_PUSHER {
                            let id = p as u64 * BATCHES_PER_PUSHER + i;
                            collector.accept_flush(Batch {
                                encoded_bytes: Vec::new(),
                                event_count: id,
                            });
                        }
                    })
                })
                .collect();

            let popper_collector = collector.clone();
            let popper = crate::primitives::thread::spawn(move || {
                let mut popped = Vec::new();
                // A few opportunistic pops racing the pushers; whatever
                // survives the race is swept by the final drain below.
                for _ in 0..BATCHES_PER_PUSHER {
                    if let Some(batch) = popper_collector.next() {
                        popped.push(batch.event_count());
                    }
                    shuttle::thread::yield_now();
                }
                popped
            });

            for p in pushers {
                p.join().unwrap();
            }
            let mut popped = popper.join().unwrap();
            // Pushers are done, so nothing more will ever be pushed: this
            // drain is final and race-free.
            while let Some(batch) = collector.next() {
                popped.push(batch.event_count());
            }

            let evicted = collector.take_dropped_batches() as u64;
            let total = PUSHERS as u64 * BATCHES_PER_PUSHER;
            assert_eq!(
                popped.len() as u64 + evicted,
                total,
                "every pushed batch must be either popped or evicted exactly once"
            );
            let mut sorted = popped.clone();
            sorted.sort_unstable();
            sorted.dedup();
            assert_eq!(
                sorted.len(),
                popped.len(),
                "a batch id was popped more than once"
            );
        }
    }
}
