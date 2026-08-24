//! dial9's macros expand to `dial9-trace-format` paths, they have to resolve
//! for callers that only depend on the facades. Compiling means they do.

use dial9::format::TraceEvent;

#[derive(TraceEvent)]
struct ViaFacade {
    #[traceevent(timestamp)]
    timestamp_ns: u64,
    #[traceevent(unit = "ms", kind = "gauge")]
    latency_ms: u64,
    label: Option<String>,
}

/// Borrowed fields take the same paths.
#[derive(TraceEvent)]
struct BorrowedViaFacade<'a> {
    #[traceevent(timestamp)]
    timestamp_ns: u64,
    path: &'a str,
}

/// `wire_slot` reaches `__NEXT_TYPE_SLOT`, which the others do not.
#[derive(TraceEvent)]
#[traceevent(wire_slot)]
struct SlottedViaFacade {
    #[traceevent(timestamp)]
    timestamp_ns: u64,
    value: u32,
}

#[test]
fn derive_resolves_through_dial9() {
    assert_eq!(ViaFacade::event_name(), "ViaFacade");
    assert_eq!(BorrowedViaFacade::event_name(), "BorrowedViaFacade");
    assert_ne!(SlottedViaFacade::type_slot(), 0);
}

/// `dial9_span!` expands a derive of its own, and the user writes none of it.
#[test]
fn dial9_span_resolves_through_dial9_utils() {
    let _span = dial9_utils::dial9_span!("facade", n: u64 = 1);
}
