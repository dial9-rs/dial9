#![cfg(feature = "pipeline")]
use dial9_core::{buffer::MemoryBuffer, pipeline::*, recorder::recorder};
use std::{
    future::{Future, IntoFuture},
    pin::Pin,
    time::Duration,
};

struct PassThrough;
impl SegmentProcessor for PassThrough {
    fn name(&self) -> &'static str {
        "PassThrough"
    }
    fn process(
        &mut self,
        d: SegmentData,
    ) -> Pin<Box<dyn Future<Output = Result<SegmentData, ProcessError>> + Send + '_>> {
        Box::pin(std::future::ready(Ok(d)))
    }
}

#[tokio::test]
async fn dump_with_max_lookforward_does_not_kill_worker() {
    let rec = recorder(MemoryBuffer::new(1 << 20).unwrap())
        .processors(vec![Box::new(PassThrough)])
        .with_dump_trigger(|_| {})
        .build();
    let trigger = rec.handle().dump_trigger().unwrap();
    let mut unbounded = trigger
        .dump_time_range(Duration::ZERO, Duration::MAX)
        .into_future();
    // Finite look-forwards just below `Instant`'s maximum: representable,
    // but too close to it for tokio's timer, which rounds the deadline up
    // with an unchecked add.
    let max = max_remaining(tokio::time::Instant::now());
    for step in 0..30 {
        drop(trigger.dump_time_range(Duration::ZERO, max - Duration::from_micros(100) * step));
    }
    let later = tokio::time::timeout(
        Duration::from_secs(3),
        trigger.dump_current_data().into_future(),
    )
    .await;
    assert!(matches!(later, Ok(Ok(_))), "worker died: {later:?}");
    // The worker registers requests in order and resolves every due dump
    // before the next one, so a MAX dump that resolved early would be ready
    // by now. An unbounded forward window stays open until shutdown.
    let still_open = tokio::time::timeout(Duration::ZERO, &mut unbounded).await;
    assert!(
        still_open.is_err(),
        "MAX look-forward dump resolved early: {still_open:?}"
    );
}

/// Largest `d` with `now.checked_add(d)` representable.
fn max_remaining(now: tokio::time::Instant) -> Duration {
    let (mut lo, mut hi) = (0u128, Duration::MAX.as_nanos());
    while lo < hi {
        let mid = lo + (hi - lo).div_ceil(2);
        if now.checked_add(from_nanos(mid)).is_some() {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    from_nanos(lo)
}

fn from_nanos(n: u128) -> Duration {
    Duration::new((n / 1_000_000_000) as u64, (n % 1_000_000_000) as u32)
}
