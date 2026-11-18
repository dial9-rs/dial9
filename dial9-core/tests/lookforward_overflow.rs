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
    drop(trigger.dump_time_range(Duration::ZERO, Duration::MAX));
    let later = tokio::time::timeout(
        Duration::from_secs(3),
        trigger.dump_current_data().into_future(),
    )
    .await;
    assert!(matches!(later, Ok(Ok(_))), "worker died: {later:?}");
}
