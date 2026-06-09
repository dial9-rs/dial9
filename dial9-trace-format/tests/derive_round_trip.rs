//! Round-trip tests: encode via derive macro, decode via serde deserializer.

use dial9_trace_format::decoder::Decoder;
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
