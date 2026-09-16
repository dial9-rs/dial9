//! Rate limiting for log lines.
//!
//! Adapted from metrique-writer-format-emf's rate_limit module.

use std::sync::OnceLock;
use std::time::{Duration, Instant};

#[doc(hidden)]
pub fn time_since_epoch() -> Duration {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    Instant::now().duration_since(*EPOCH.get_or_init(Instant::now))
}

#[doc(hidden)]
pub fn next_call_secs(now: Duration, interval: Duration) -> u64 {
    now.checked_add(interval).unwrap_or(Duration::MAX).as_secs()
}

/// Evaluate `$call` at most once every `$interval` per call site.
///
/// With `key = $key`, the limit is per distinct `$key`s through the
/// same call site.
#[macro_export]
macro_rules! rate_limited {
    ($interval:expr, $call:expr) => {{
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT_CALL: AtomicU64 = AtomicU64::new(u64::MIN);
        let interval = $interval;
        let time = $crate::rate_limit::time_since_epoch();
        let next = NEXT_CALL.load(Ordering::Relaxed);
        if next <= time.as_secs() {
            let new_next = $crate::rate_limit::next_call_secs(time, interval);
            if NEXT_CALL
                .compare_exchange(next, new_next, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
            {
                $call;
            }
        }
    }};
    ($interval:expr, key = $key:expr, $call:expr) => {{
        static NEXT_CALLS: std::sync::LazyLock<
            std::sync::Mutex<std::collections::HashMap<&'static str, u64>>,
        > = std::sync::LazyLock::new(Default::default);
        let interval = $interval;
        let key: &'static str = $key;
        let time = $crate::rate_limit::time_since_epoch();
        let mut next_calls = NEXT_CALLS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let now = time.as_secs();
        let new_next = $crate::rate_limit::next_call_secs(time, interval);
        let mut ready = false;
        next_calls
            .entry(key)
            .and_modify(|next| {
                if *next <= now {
                    ready = true;
                    *next = new_next;
                }
            })
            .or_insert_with(|| {
                ready = true;
                new_next
            });
        drop(next_calls);
        if ready {
            $call;
        }
    }};
}

pub use crate::rate_limited;
