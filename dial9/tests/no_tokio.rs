use dial9::{Encodable, ThreadLocalEncoder, current_handle, record_event};

struct Probe;

impl Encodable for Probe {
    fn encode(&self, _encoder: &mut ThreadLocalEncoder<'_>) {}
}

#[test]
fn record_event_without_session_is_a_noop() {
    // No `set_tl_handle` on this thread -> disabled handle.
    assert!(!current_handle().is_enabled());
    // Must not panic, and must not require a Tokio runtime.
    record_event(Probe);
}
