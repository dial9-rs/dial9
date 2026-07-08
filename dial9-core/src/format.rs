//! Wire-format events emitted by the bus itself.
use dial9_trace_format::TraceEvent;

crate::test_util_pub! {
/// Segment metadata as key/value entries, written when a segment is sealed.
#[derive(TraceEvent)]
#[traceevent(wire_slot)]
struct SegmentMetadataEvent {
    #[traceevent(timestamp)]
    pub timestamp_ns: u64,
    pub entries: Vec<(String, String)>,
}
}

crate::test_util_pub! {
/// Clock-correlation anchor. `timestamp_ns` (monotonic) and `realtime_ns`
/// (nanoseconds since Unix epoch) are captured at the same instant via
/// [`clock_pair`], so offline consumers can recover wall clock from the
/// monotonic event stream.
///
/// [`clock_pair`]: crate::clock::clock_pair
#[derive(TraceEvent)]
#[traceevent(wire_slot)]
struct ClockSyncEvent {
    #[traceevent(timestamp)]
    pub timestamp_ns: u64,
    pub realtime_ns: u64,
}
}
