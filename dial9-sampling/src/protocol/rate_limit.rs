//! Per-call-site throttling for maintenance-loop diagnostics.

macro_rules! rate_limited {
    ($interval:expr, $call:expr) => {{
        use std::{
            sync::{
                OnceLock,
                atomic::{AtomicU64, Ordering},
            },
            time::Instant,
        };
        static EPOCH: OnceLock<Instant> = OnceLock::new();
        static NEXT_CALL: AtomicU64 = AtomicU64::new(0);
        let now = Instant::now();
        let elapsed = now.saturating_duration_since(*EPOCH.get_or_init(|| now));
        let next = NEXT_CALL.load(Ordering::Relaxed);
        if next <= elapsed.as_secs() {
            let new_next = elapsed.saturating_add($interval).as_secs();
            if NEXT_CALL
                .compare_exchange(next, new_next, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
            {
                $call;
            }
        }
    }};
}

pub(crate) use rate_limited;
