//! The tracing layer against the process-global `Dial9Handle`.

use dial9_core::buffer::DiskBuffer;
use dial9_core::handle::Dial9Handle;
use dial9_core::recorder::recorder;
use dial9_utils::tracing_layer::Dial9TracingLayer;
use std::time::Duration;
use tracing_subscriber::prelude::*;

/// Spans opened on a thread no runtime owns must also be closed there.
///
/// `on_close` resolving a narrower set of handles than enter/exit would leave
/// the trace with unbalanced spans.
#[test]
fn spans_on_a_plain_thread_are_balanced() {
    let dir = tempfile::tempdir().unwrap();
    let writer = DiskBuffer::single_file(dir.path().join("trace.bin")).unwrap();
    let recorder = recorder(writer).build();
    recorder.install_global();

    std::thread::spawn(|| {
        assert!(
            Dial9Handle::try_current().is_none(),
            "no thread-local handle here, so the layer must be resolving the global"
        );

        let subscriber = tracing_subscriber::registry().with(Dial9TracingLayer::new());
        let _guard = tracing::subscriber::set_default(subscriber);

        let span = tracing::info_span!("plain_thread_span", key = "value");
        span.in_scope(|| {});
        drop(span);
    })
    .join()
    .expect("worker thread");

    recorder.graceful_shutdown(Duration::from_secs(1));

    let data = std::fs::read(dir.path().join("trace.0.bin")).unwrap();
    let mut decoder = dial9_trace_format::decoder::Decoder::new(&data).unwrap();
    let (mut enters, mut exits, mut closes) = (0u32, 0u32, 0u32);
    decoder
        .for_each_event(|ev| {
            if ev.name.starts_with("SpanEnter:") {
                enters += 1;
            } else if ev.name.starts_with("SpanExit:") {
                exits += 1;
            } else if ev.name == "SpanCloseEvent" {
                closes += 1;
            }
        })
        .expect("decode events");

    assert_eq!(enters, 1, "the span should be entered once");
    assert_eq!(exits, enters, "every span entered should also be exited");
    assert_eq!(closes, enters, "and closed");
}
