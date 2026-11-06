//! Same-named Rust event types retain independent schemas and wire IDs.

use dial9_trace_format::TraceEvent;
use dial9_trace_format::decoder::{DecodedFrame, Decoder};
use dial9_trace_format::encoder::{Encoder, Schema};
use dial9_trace_format::schema::FieldDef;
use dial9_trace_format::types::{FieldType, FieldValue};

mod a {
    #[derive(dial9_trace_format::TraceEvent)]
    pub struct Ev {
        #[traceevent(timestamp)]
        pub ts: u64,
        pub x: u64,
    }
}

mod b {
    #[derive(dial9_trace_format::TraceEvent)]
    pub struct Ev {
        #[traceevent(timestamp)]
        pub ts: u64,
        pub y: u64,
        pub z: u64,
    }
}

#[derive(TraceEvent)]
#[traceevent(name = "Ev")]
struct Annotated {
    #[traceevent(timestamp)]
    ts: u64,
    #[traceevent(unit = "us")]
    x: u64,
}

fn write_colliding_events(enc: &mut Encoder) {
    enc.write_infallible(&a::Ev { ts: 1, x: 10 });
    enc.write_infallible(&b::Ev {
        ts: 2,
        y: 20,
        z: 30,
    });
    enc.write_infallible(&a::Ev { ts: 3, x: 40 });
    enc.write_infallible(&b::Ev {
        ts: 4,
        y: 50,
        z: 60,
    });
}

fn assert_colliding_events(bytes: &[u8]) {
    let mut decoder = Decoder::new(bytes).unwrap();
    let mut events = Vec::new();
    decoder
        .for_each_event(|event| {
            assert_eq!(event.name, "Ev");
            events.push((
                event.type_id,
                event.timestamp_ns,
                event.field_names().map(str::to_owned).collect::<Vec<_>>(),
                event.deserialize::<serde_json::Value>().unwrap(),
            ));
        })
        .unwrap();
    assert_eq!(decoder.registry().entries().count(), 2);
    assert_eq!(events.len(), 4);
    assert_ne!(events[0].0, events[1].0);
    assert_eq!(events[0].0, events[2].0);
    assert_eq!(events[1].0, events[3].0);
    assert_eq!(events[0].1, 1);
    assert_eq!(events[1].1, 2);
    assert_eq!(events[2].1, 3);
    assert_eq!(events[3].1, 4);
    assert_eq!(events[0].2, ["x"]);
    assert_eq!(events[1].2, ["y", "z"]);
    assert_eq!(events[0].3["x"], 10);
    assert_eq!(events[1].3["y"], 20);
    assert_eq!(events[1].3["z"], 30);
    assert_eq!(events[2].3["x"], 40);
    assert_eq!(events[3].3["y"], 50);
    assert_eq!(events[3].3["z"], 60);
}

#[test]
fn same_names_with_different_fields_round_trip_after_reset() {
    let mut enc = Encoder::new();
    write_colliding_events(&mut enc);
    assert_colliding_events(&enc.reset_to(Vec::new()).unwrap());
    write_colliding_events(&mut enc);
    assert_colliding_events(&enc.finish());
}

#[test]
fn same_names_with_different_annotations_round_trip() {
    let mut enc = Encoder::new();
    enc.write_infallible(&a::Ev { ts: 1, x: 10 });
    enc.write_infallible(&Annotated { ts: 2, x: 20 });
    enc.write_infallible(&Annotated { ts: 3, x: 30 });
    let bytes = enc.finish();
    let mut decoder = Decoder::new(&bytes).unwrap();
    let mut ids = Vec::new();
    decoder
        .for_each_event(|event| {
            assert_eq!(event.name, "Ev");
            ids.push(event.type_id);
            if event.timestamp_ns == 1 {
                assert!(event.schema.annotations().is_empty());
            } else {
                let annotations = event.schema.annotations();
                assert_eq!(annotations.len(), 1);
                assert_eq!(annotations[0].field_index(), 0);
                assert_eq!(annotations[0].key(), "unit");
                assert_eq!(annotations[0].value(), "us");
            }
        })
        .unwrap();
    assert_eq!(ids.len(), 3);
    assert_ne!(ids[0], ids[1]);
    assert_eq!(ids[1], ids[2]);
}

#[test]
fn typed_and_manual_schemas_with_same_name_are_independent() {
    let mut enc = Encoder::new();
    let schema = Schema::new("Ev", vec![FieldDef::new("manual", FieldType::Bool)]);
    enc.write_event(&schema, 1, &[FieldValue::Bool(true)])
        .unwrap();
    enc.write_infallible(&a::Ev { ts: 2, x: 10 });
    enc.write_event(&schema, 3, &[FieldValue::Bool(false)])
        .unwrap();
    let bytes = enc.finish();
    let mut decoder = Decoder::new(&bytes).unwrap();
    let mut events = Vec::new();
    decoder
        .for_each_event(|event| {
            events.push((
                event.type_id,
                event.deserialize::<serde_json::Value>().unwrap(),
            ));
        })
        .unwrap();
    assert_eq!(events.len(), 3);
    assert_ne!(events[0].0, events[1].0);
    assert_eq!(events[0].0, events[2].0);
    assert_eq!(events[0].1["manual"], true);
    assert_eq!(events[1].1["x"], 10);
    assert_eq!(events[2].1["manual"], false);
}

#[test]
fn colliding_types_can_be_appended_to_a_decoded_trace() {
    let mut enc = Encoder::new();
    enc.write_infallible(&a::Ev { ts: 1, x: 10 });
    let mut bytes = enc.finish();
    let mut decoder = Decoder::new(&bytes).unwrap();
    decoder.for_each_event(|_| {}).unwrap();
    let mut suffix = Vec::new();
    let mut enc = decoder.into_encoder(&mut suffix);
    enc.write(&b::Ev {
        ts: 2,
        y: 20,
        z: 30,
    })
    .unwrap();
    drop(enc);
    bytes.extend(suffix);
    let mut decoder = Decoder::new(&bytes).unwrap();
    let frames = decoder.decode_all();
    let events: Vec<_> = frames
        .into_iter()
        .filter_map(|frame| match frame {
            DecodedFrame::Event {
                type_id,
                timestamp_ns,
                values,
            } => Some((type_id, timestamp_ns, values)),
            _ => None,
        })
        .collect();
    assert_eq!(events.len(), 2);
    assert_ne!(events[0].0, events[1].0);
    assert_eq!(events[0].1, 1);
    assert_eq!(events[1].1, 2);
    assert_eq!(events[0].2, [FieldValue::Varint(10)]);
    assert_eq!(
        events[1].2,
        [FieldValue::Varint(20), FieldValue::Varint(30)]
    );
}
