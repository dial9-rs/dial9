//! Worker-local Bernoulli sampling, calibrated from completed one-second epochs.

use crate::primitives::sync::{
    Arc, Mutex,
    atomic::{AtomicU64, Ordering},
};
use crate::telemetry::TaskDumpConfig;
use crossbeam_utils::CachePadded;
use dial9_core::sampling::SplitMix64;

const EPOCH_NS: u64 = 1_000_000_000;

/// One sampler per logical worker, including when its core changes threads.
/// A block_in_place caller can finish a poll concurrently with the replacement
/// worker. Serialize only the sampling decision, never application polls or capture.
pub(crate) struct WorkerTaskDumpSampler {
    // Keep hot writes off the cache lines used by Arc counts and source metadata.
    sampler: CachePadded<Mutex<TaskDumpSampler>>,
    sampling_active_ns: AtomicU64,
    activated_workers: Arc<AtomicU64>,
}

impl WorkerTaskDumpSampler {
    pub(crate) fn new(
        config: TaskDumpConfig,
        worker_id: u64,
        now: u64,
        activated_workers: Arc<AtomicU64>,
    ) -> Self {
        Self {
            sampler: CachePadded::new(Mutex::new(TaskDumpSampler::new(config, worker_id, now))),
            sampling_active_ns: AtomicU64::new(0),
            activated_workers,
        }
    }

    pub(crate) fn observe_pending(&self, now: impl FnOnce() -> u64) -> Option<f64> {
        let mut sampler = self.sampler.lock().unwrap();
        let calibrating = sampler.sampling_active_ns.is_none();
        // Read at the pending transition, after acquiring the worker state;
        // poll-start timestamps may precede this epoch by a long application poll.
        let selected = sampler.observe_pending(now());
        if calibrating && let Some(timestamp) = sampler.sampling_active_ns {
            self.sampling_active_ns.store(timestamp, Ordering::Relaxed);
            // One release per worker lets the source detect new metadata
            // without locking or scanning worker samplers on every flush.
            self.activated_workers.fetch_add(1, Ordering::Release);
        }
        selected
    }

    /// The source reads only this atomic, without locking the sampling decision.
    pub(crate) fn sampling_active_ns(&self) -> Option<u64> {
        std::num::NonZeroU64::new(self.sampling_active_ns.load(Ordering::Relaxed))
            .map(std::num::NonZeroU64::get)
    }
}

pub(crate) struct TaskDumpSampler {
    epoch_start: u64,
    eligible: u64,
    target_rate: f64,
    probability: f64,
    skip: u64,
    rng: SplitMix64,
    pub(crate) sampling_active_ns: Option<u64>,
}

impl TaskDumpSampler {
    pub(crate) fn new(config: TaskDumpConfig, worker_id: u64, now: u64) -> Self {
        let seed = config.rng_seed().unwrap_or(now) ^ worker_id.wrapping_mul(0x517cc1b727220a95);
        Self {
            epoch_start: now,
            eligible: 0,
            target_rate: config.captures_per_second_per_worker(),
            probability: 0.0,
            skip: 0,
            rng: SplitMix64::new(seed),
            sampling_active_ns: None,
        }
    }

    /// Returns the probability used for this transition only when selected.
    /// The current transition never contributes to its own rate estimate.
    pub(crate) fn observe_pending(&mut self, now: u64) -> Option<f64> {
        let elapsed = now.saturating_sub(self.epoch_start);
        if elapsed >= EPOCH_NS {
            self.advance_epoch(now, elapsed);
        }
        self.eligible += 1;
        if self.probability == 0.0 {
            return None;
        }
        if self.skip != 0 {
            self.skip -= 1;
            return None;
        }
        self.skip = self.draw_skip();
        Some(self.probability)
    }

    #[cold]
    fn advance_epoch(&mut self, now: u64, elapsed: u64) {
        let epochs = elapsed / EPOCH_NS;
        // If whole epochs passed without a transition, the immediately
        // preceding epoch was empty. A quiet worker can sample every transition.
        let previous_count = if epochs == 1 { self.eligible } else { 0 };
        self.probability = if previous_count == 0 {
            1.0
        } else {
            (self.target_rate / previous_count as f64).min(1.0)
        };
        self.epoch_start += epochs * EPOCH_NS;
        self.eligible = 0;
        self.skip = self.draw_skip();
        self.sampling_active_ns.get_or_insert(now);
    }

    fn draw_skip(&mut self) -> u64 {
        if self.probability == 1.0 {
            return 0;
        }
        // Inverse geometric CDF: failures before a Bernoulli success.
        // u is in (0, 1]; ln_1p preserves precision for small probabilities.
        let u = ((self.rng.next_u64() >> 11) + 1) as f64 / (1u64 << 53) as f64;
        (u.ln() / (-self.probability).ln_1p()).floor() as u64
    }
}

#[cfg(all(test, not(shuttle)))]
mod tests {
    use super::*;

    fn sampler(rate: u32, worker: u64) -> TaskDumpSampler {
        TaskDumpSampler::new(
            TaskDumpConfig::builder()
                .captures_per_second_per_worker(rate)
                .rng_seed(42)
                .build(),
            worker,
            0,
        )
    }

    #[test]
    fn calibration_and_epoch_probability() {
        let mut s = sampler(10, 0);
        for i in 0..100 {
            assert_eq!(s.observe_pending(i * EPOCH_NS / 100), None);
        }
        assert_eq!(s.sampling_active_ns, None);
        s.observe_pending(EPOCH_NS);
        assert_eq!(s.probability, 0.1);
        assert_eq!(s.sampling_active_ns, Some(EPOCH_NS));
        s.observe_pending(2 * EPOCH_NS);
        assert_eq!(s.probability, 1.0);
        assert_eq!(s.sampling_active_ns, Some(EPOCH_NS));
        assert_eq!(s.observe_pending(5 * EPOCH_NS), Some(1.0));
    }

    #[test]
    fn rates_converge_across_workers_and_poll_rates() {
        for target in [1, 10, 40] {
            for polls_per_second in [5, 100, 1_000] {
                for worker in [0, 1, 17] {
                    let mut s = sampler(target, worker);
                    let seconds = 2_000;
                    let mut count = 0;
                    for i in 0..(seconds + 1) * polls_per_second {
                        if let Some(p) = s.observe_pending(i * EPOCH_NS / polls_per_second) {
                            assert_eq!(p, (f64::from(target) / polls_per_second as f64).min(1.0));
                            count += 1;
                        }
                    }
                    let expected = seconds * u64::from(target).min(polls_per_second);
                    assert!(
                        (count as f64 / expected as f64 - 1.0).abs() < 0.1,
                        "target={target} polls={polls_per_second} worker={worker}: {count}/{expected}"
                    );
                }
            }
        }
    }

    #[test]
    fn inverse_probability_recovers_waits_across_changing_rates() {
        let mut s = sampler(20, 0);
        let mut actual = [0.0; 2];
        let mut estimated = [0.0; 2];
        for epoch in 0..10_000 {
            let polls = if epoch % 2 == 0 { 100 } else { 1_000 };
            for i in 0..polls {
                let selected = s.observe_pending(epoch * EPOCH_NS + i * EPOCH_NS / polls);
                if epoch == 0 {
                    continue;
                }
                // Mix two tasks with unequal idle durations on the same worker.
                let task = (i % 2) as usize;
                let idle = if task == 0 { 3.0 } else { 97.0 };
                actual[task] += idle;
                if let Some(p) = selected {
                    estimated[task] += idle / p;
                }
            }
        }
        for task in 0..2 {
            assert!((estimated[task] / actual[task] - 1.0).abs() < 0.02);
        }
    }

    #[test]
    fn seeds_are_reproducible_and_distinct_per_worker() {
        let sequence = |worker| {
            let mut s = sampler(10, worker);
            (0..10_000)
                .map(|i| s.observe_pending(i * EPOCH_NS / 1_000))
                .collect::<Vec<_>>()
        };
        assert_eq!(sequence(2), sequence(2));
        assert_ne!(sequence(2), sequence(3));
    }
}

#[cfg(all(test, shuttle))]
mod shuttle_tests {
    use super::*;
    use crate::primitives::{sync::Arc, thread};

    dial9_core::shuttle_test! {
        num_iters = 1_000, depth = 3;
        fn concurrent_sampling_and_metadata() {
            let activated = Arc::new(AtomicU64::new(0));
            let worker = Arc::new(WorkerTaskDumpSampler::new(
                TaskDumpConfig::default(), 0, 0, activated.clone(),
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
                    assert_eq!(worker.sampling_active_ns(), Some(EPOCH_NS));
                }
                if let Some(active) = worker.sampling_active_ns() {
                    assert_eq!(active, EPOCH_NS);
                }
                shuttle::thread::yield_now();
            }
            for poller in pollers { poller.join().unwrap(); }
            assert_eq!(worker.sampler.lock().unwrap().eligible, 6);
            assert_eq!(worker.sampling_active_ns(), Some(EPOCH_NS));
            assert_eq!(activated.load(Ordering::Acquire), 1);
        }
    }
}
