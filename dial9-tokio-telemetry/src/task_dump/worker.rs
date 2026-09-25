use super::sampler::TaskSampler;
use crate::primitives::sync::{
    Arc, Mutex,
    atomic::{AtomicU64, Ordering},
};
use crate::telemetry::TaskSamplingConfig;
use crossbeam_utils::CachePadded;

/// One sampler per logical worker, including when its core changes threads.
/// A block_in_place caller can finish a poll concurrently with the replacement
/// worker. Serialize only the sampling decision, never application polls or capture.
pub(crate) struct WorkerSampler {
    // Keep hot writes off the cache lines used by Arc counts and source metadata.
    sampler: CachePadded<Mutex<TaskSampler>>,
    sampling_started_at_ns: AtomicU64,
    activated_workers: Arc<AtomicU64>,
}

impl WorkerSampler {
    pub(crate) fn new(
        config: TaskSamplingConfig,
        worker_id: u64,
        now: u64,
        activated_workers: Arc<AtomicU64>,
    ) -> Self {
        Self {
            sampler: CachePadded::new(Mutex::new(TaskSampler::new(config, worker_id, now))),
            sampling_started_at_ns: AtomicU64::new(0),
            activated_workers,
        }
    }

    pub(crate) fn observe_pending(&self, now: impl FnOnce() -> u64) -> Option<f64> {
        let mut sampler = self.sampler.lock().unwrap();
        let calibrating = sampler.sampling_started_at_ns.is_none();
        // Read at the pending transition, after acquiring the worker state;
        // poll-start timestamps may precede this epoch by a long application poll.
        let selected = sampler.observe_pending(now());
        if calibrating && let Some(timestamp) = sampler.sampling_started_at_ns {
            self.sampling_started_at_ns
                .store(timestamp, Ordering::Relaxed);
            // One release per worker lets the source detect new metadata
            // without locking or scanning worker samplers on every flush.
            self.activated_workers.fetch_add(1, Ordering::Release);
        }
        selected
    }

    /// The source reads only this atomic, without locking the sampling decision.
    pub(crate) fn sampling_started_at_ns(&self) -> Option<u64> {
        std::num::NonZeroU64::new(self.sampling_started_at_ns.load(Ordering::Relaxed))
            .map(std::num::NonZeroU64::get)
    }
}

#[cfg(all(test, shuttle))]
mod shuttle_tests {
    use super::super::sampler::EPOCH_NS;
    use super::*;
    use crate::primitives::{sync::Arc, thread};

    dial9_core::shuttle_test! {
        num_iters = 1_000, depth = 3;
        fn concurrent_sampling_and_metadata() {
            let activated = Arc::new(AtomicU64::new(0));
            let worker = Arc::new(WorkerSampler::new(
                TaskSamplingConfig::default(), 0, 0, activated.clone(),
            ));
            let clock = Arc::new(AtomicU64::new(EPOCH_NS));
            let pollers: Vec<_> = (0..2).map(|_| {
                let worker = worker.clone();
                let clock = clock.clone();
                thread::spawn(move || {
                    for _ in 0..3 {
                        assert_eq!(worker.observe_pending(|| clock.fetch_add(1, Ordering::Relaxed)), Some(1.0));
                    }
                })
            }).collect();
            for _ in 0..3 {
                if activated.load(Ordering::Acquire) == 1 {
                    assert_eq!(worker.sampling_started_at_ns(), Some(EPOCH_NS));
                }
                if let Some(active) = worker.sampling_started_at_ns() {
                    assert_eq!(active, EPOCH_NS);
                }
                shuttle::thread::yield_now();
            }
            for poller in pollers { poller.join().unwrap(); }
            assert_eq!(worker.sampler.lock().unwrap().eligible, 6);
            assert_eq!(worker.sampling_started_at_ns(), Some(EPOCH_NS));
            assert_eq!(activated.load(Ordering::Acquire), 1);
        }
    }
}
