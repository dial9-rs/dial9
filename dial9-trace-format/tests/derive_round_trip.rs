//! Round-trip tests: encode via derive macro, decode via serde deserializer.

use dial9_trace_format::decoder::{DecodedFrame, Decoder};
use dial9_trace_format::encoder::Encoder;
use dial9_trace_format::{InternedString, TraceEvent};
use serde::Deserialize;

#[derive(TraceEvent)]
struct SimpleEvent {
    #[traceevent(timestamp)]
    timestamp_ns: u64,
    value: u32,
    name: String,
}

#[derive(TraceEvent)]
struct PooledEvent {
    #[traceevent(timestamp)]
    timestamp_ns: u64,
    label: InternedString,
    frames: dial9_trace_format::InternedStackFrames,
}

#[derive(TraceEvent)]
struct OptionalEvent {
    #[traceevent(timestamp)]
    timestamp_ns: u64,
    maybe_val: Option<u64>,
    maybe_str: Option<InternedString>,
}

/// Borrows its fields instead of owning them — written via `write_borrowed`.
#[derive(TraceEvent)]
struct BorrowedEvent<'a> {
    #[traceevent(timestamp)]
    timestamp_ns: u64,
    path: &'a str,
    body: &'a [u8],
}

/// Owned twin of `BorrowedEvent`, used to prove identical field encoding.
#[derive(TraceEvent)]
struct OwnedTwin {
    #[traceevent(timestamp)]
    timestamp_ns: u64,
    path: String,
    body: Vec<u8>,
}

#[derive(Debug, Deserialize, PartialEq)]
#[serde(tag = "event")]
enum Decoded {
    SimpleEvent {
        timestamp_ns: u64,
        value: u32,
        name: String,
    },
    PooledEvent {
        timestamp_ns: u64,
        label: String,
        frames: Vec<u64>,
    },
    OptionalEvent {
        timestamp_ns: u64,
        maybe_val: Option<u64>,
        maybe_str: Option<String>,
    },
    #[serde(other)]
    Other,
}

#[test]
fn simple_round_trip() {
    let mut enc = Encoder::new_to(Vec::new()).unwrap();
    enc.write_infallible(&SimpleEvent {
        timestamp_ns: 1_000_000,
        value: 42,
        name: "hello".into(),
    });
    let buf = enc.into_inner();

    let mut dec = Decoder::new(&buf).unwrap();
    let mut events = Vec::new();
    dec.for_each_event(|raw| {
        events.push(raw.deserialize::<Decoded>().unwrap());
    })
    .unwrap();

    assert_eq!(events.len(), 1);
    assert_eq!(
        events[0],
        Decoded::SimpleEvent {
            timestamp_ns: 1_000_000,
            value: 42,
            name: "hello".into(),
        }
    );
}

#[test]
fn pooled_round_trip() {
    let mut enc = Encoder::new_to(Vec::new()).unwrap();
    let label = enc.intern_string("my-label").unwrap();
    let frames = enc.intern_stack_frames(&[0x1000, 0x2000]).unwrap();
    enc.write_infallible(&PooledEvent {
        timestamp_ns: 2_000_000,
        label,
        frames,
    });
    let buf = enc.into_inner();

    let mut dec = Decoder::new(&buf).unwrap();
    let mut events = Vec::new();
    dec.for_each_event(|raw| {
        events.push(raw.deserialize::<Decoded>().unwrap());
    })
    .unwrap();

    assert_eq!(events.len(), 1);
    assert_eq!(
        events[0],
        Decoded::PooledEvent {
            timestamp_ns: 2_000_000,
            label: "my-label".into(),
            frames: vec![0x1000, 0x2000],
        }
    );
}

#[derive(Debug, Deserialize, PartialEq)]
struct DecodedBorrowed {
    timestamp_ns: u64,
    path: String,
    body: Vec<u8>,
}

#[test]
fn borrowed_event_round_trip() {
    let path = String::from("/api/object");
    let body = vec![1u8, 2, 3];

    let mut enc = Encoder::new_to(Vec::new()).unwrap();
    enc.write_infallible(&BorrowedEvent {
        timestamp_ns: 5_000_000,
        path: &path,
        body: &body,
    });
    let buf = enc.into_inner();

    let mut dec = Decoder::new(&buf).unwrap();
    let mut events = Vec::new();
    dec.for_each_event(|raw| {
        events.push(raw.deserialize::<DecodedBorrowed>().unwrap());
    })
    .unwrap();

    assert_eq!(events.len(), 1);
    assert_eq!(
        events[0],
        DecodedBorrowed {
            timestamp_ns: 5_000_000,
            path: "/api/object".into(),
            body: vec![1, 2, 3],
        }
    );
}

#[test]
fn borrowed_event_reuses_schema_across_lifetimes() {
    let mut enc = Encoder::new_to(Vec::new()).unwrap();

    {
        let path = String::from("/first");
        let body = vec![1u8];
        enc.write_infallible(&BorrowedEvent {
            timestamp_ns: 5_000_000,
            path: &path,
            body: &body,
        });
    }
    {
        let path = String::from("/second");
        let body = vec![2u8];
        enc.write_infallible(&BorrowedEvent {
            timestamp_ns: 6_000_000,
            path: &path,
            body: &body,
        });
    }

    let buf = enc.into_inner();
    let mut dec = Decoder::new(&buf).unwrap();
    let frames = dec.decode_all();
    assert_eq!(
        frames
            .iter()
            .filter(|frame| matches!(frame, DecodedFrame::Schema(schema) if schema.name() == "BorrowedEvent"))
            .count(),
        1,
        "all lifetime instantiations must share one schema"
    );
    assert_eq!(
        frames
            .iter()
            .filter(|frame| matches!(frame, DecodedFrame::Event { .. }))
            .count(),
        2
    );
}

/// A borrowed event and its owned twin must encode the event field payload
/// byte-identically: `&str`/`String` and `&[u8]`/`Vec<u8>` share a wire type.
#[test]
fn borrowed_matches_owned_field_bytes() {
    let path = String::from("/api/object");
    let body = vec![9u8, 8, 7, 6];

    let mut enc_b = Encoder::new_to(Vec::new()).unwrap();
    enc_b.write_infallible(&BorrowedEvent {
        timestamp_ns: 6_000_000,
        path: &path,
        body: &body,
    });
    let bytes_b = enc_b.into_inner();

    let mut enc_o = Encoder::new_to(Vec::new()).unwrap();
    enc_o.write_infallible(&OwnedTwin {
        timestamp_ns: 6_000_000,
        path,
        body,
    });
    let bytes_o = enc_o.into_inner();

    // The schema frames differ only in the event name, so the buffers differ in
    // length. But each type is the sole schema, so both get the same wire id,
    // and the trailing event frame (tag + wire id + timestamp delta + field
    // payload) is byte-identical. Compare their longest common suffix and assert
    // it covers the whole (equal-length) event frame.
    let common_suffix = bytes_b
        .iter()
        .rev()
        .zip(bytes_o.iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    // Event frame = 1 (tag) + 2 (wire id) + 3 (u24 ts delta) + payload. The
    // payload here is two length-prefixed values well over a few bytes, so a
    // common suffix of >= 6 proves the whole event frame matched.
    assert!(
        common_suffix >= 6 + "/api/object".len() + 4,
        "borrowed and owned event frames diverged; common suffix = {common_suffix}"
    );
}

#[test]
fn optional_fields_round_trip() {
    let mut enc = Encoder::new_to(Vec::new()).unwrap();
    let s = enc.intern_string("present").unwrap();
    enc.write_infallible(&OptionalEvent {
        timestamp_ns: 3_000_000,
        maybe_val: Some(99),
        maybe_str: Some(s),
    });
    enc.write_infallible(&OptionalEvent {
        timestamp_ns: 4_000_000,
        maybe_val: None,
        maybe_str: None,
    });
    let buf = enc.into_inner();

    let mut dec = Decoder::new(&buf).unwrap();
    let mut events = Vec::new();
    dec.for_each_event(|raw| {
        events.push(raw.deserialize::<Decoded>().unwrap());
    })
    .unwrap();

    assert_eq!(events.len(), 2);
    assert_eq!(
        events[0],
        Decoded::OptionalEvent {
            timestamp_ns: 3_000_000,
            maybe_val: Some(99),
            maybe_str: Some("present".into()),
        }
    );
    assert_eq!(
        events[1],
        Decoded::OptionalEvent {
            timestamp_ns: 4_000_000,
            maybe_val: None,
            maybe_str: None,
        }
    );
}
