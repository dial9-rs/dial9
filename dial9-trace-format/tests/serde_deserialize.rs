//! Tests for the [`dial9_trace_format::de`] module — serde Deserializer for
//! `RawEvent`.
//!
//! These tests cover the full matrix of behaviors:
//!
//! - Round-trip encode → decode → deserialize for all field types
//!   (varints, signed/unsigned, bool, floats, strings, pooled strings,
//!   stack frames, pooled stack frames).
//! - Optional fields, both present and absent, deserialize correctly.
//! - `#[serde(other)]` catches unknown event names.
//! - Required-field schema mismatch produces an error from the deserializer.
//! - Type coercion failures produce an error from the deserializer.
//! - End-to-end test using `#[derive(TraceEvent)]` for the encode side and
//!   `#[derive(serde::Deserialize)]` for the decode side.

use dial9_trace_format::decoder::Decoder;
use dial9_trace_format::encoder::Encoder;
use dial9_trace_format::schema::FieldDef;
use dial9_trace_format::types::{FieldType, FieldValue};
use dial9_trace_format::{InternedStackFrames, InternedString, StackFrames, TraceEvent};
use serde::Deserialize;

// ── Helpers ────────────────────────────────────────────────────────────────

/// Decode the first event in `bytes` and deserialize into `T`.
///
/// Panics if no events are present in the trace or if deserialization fails.
fn decode_first<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> T {
    let mut dec = Decoder::new(bytes).expect("valid trace header");
    let mut out: Option<T> = None;
    dec.for_each_event(|raw| {
        if out.is_none() {
            out = Some(raw.deserialize::<T>().expect("deserialize first event"));
        }
    })
    .expect("no decode error");
    out.expect("at least one event in trace")
}

/// Decode all events into a Vec<T>.
fn decode_all<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> Vec<T> {
    let mut dec = Decoder::new(bytes).expect("valid trace header");
    let mut events = Vec::new();
    dec.for_each_event(|raw| {
        events.push(raw.deserialize::<T>().expect("deserialize event"));
    })
    .expect("no decode error");
    events
}

// ── Test 1: simple varint round-trip ───────────────────────────────────────

#[derive(Debug, Deserialize, PartialEq)]
struct Simple {
    timestamp_ns: u64,
    worker_id: u64,
    task_id: u64,
}

#[test]
fn varint_round_trip() {
    let mut enc = Encoder::new();
    let schema = enc
        .register_schema(
            "Simple",
            vec![
                FieldDef::new("worker_id", FieldType::Varint),
                FieldDef::new("task_id", FieldType::Varint),
            ],
        )
        .unwrap();
    enc.write_event(
        &schema,
        &[
            FieldValue::Varint(1_000_000_000),
            FieldValue::Varint(7),
            FieldValue::Varint(42),
        ],
    )
    .unwrap();
    let bytes = enc.finish();

    let event: Simple = decode_first(&bytes);
    assert_eq!(
        event,
        Simple {
            timestamp_ns: 1_000_000_000,
            worker_id: 7,
            task_id: 42,
        }
    );
}

// ── Test 2: pooled string resolves transparently ────────────────────────────

#[derive(Debug, Deserialize, PartialEq)]
struct WithPool {
    timestamp_ns: u64,
    label: String,
}

#[test]
fn pooled_string_resolves_to_string() {
    let mut enc = Encoder::new();
    let interned = enc.intern_string("worker-thread-3").unwrap();
    let schema = enc
        .register_schema(
            "WithPool",
            vec![FieldDef::new("label", FieldType::PooledString)],
        )
        .unwrap();
    enc.write_event(
        &schema,
        &[
            FieldValue::Varint(2_000),
            FieldValue::PooledString(interned),
        ],
    )
    .unwrap();
    let bytes = enc.finish();

    let event: WithPool = decode_first(&bytes);
    assert_eq!(event.timestamp_ns, 2_000);
    assert_eq!(event.label, "worker-thread-3");
}

// ── Test 3: pooled stack frames resolve to Vec<u64> ─────────────────────────

#[derive(Debug, Deserialize, PartialEq)]
struct WithStack {
    timestamp_ns: u64,
    callchain: Vec<u64>,
}

#[test]
fn pooled_stack_frames_resolve_to_vec_u64() {
    let mut enc = Encoder::new();
    let stack = enc
        .intern_stack_frames(&[0xdead_beef, 0xcafe_babe, 0x1234_5678])
        .unwrap();
    let schema = enc
        .register_schema(
            "WithStack",
            vec![FieldDef::new("callchain", FieldType::PooledStackFrames)],
        )
        .unwrap();
    enc.write_event(
        &schema,
        &[
            FieldValue::Varint(5_000),
            FieldValue::PooledStackFrames(stack),
        ],
    )
    .unwrap();
    let bytes = enc.finish();

    let event: WithStack = decode_first(&bytes);
    assert_eq!(event.timestamp_ns, 5_000);
    assert_eq!(event.callchain, vec![0xdead_beef, 0xcafe_babe, 0x1234_5678]);
}

// ── Test 4: non-pooled StackFrames also deserializes as Vec<u64> ────────────

#[test]
fn inline_stack_frames_deserialize_as_vec_u64() {
    let mut enc = Encoder::new();
    let schema = enc
        .register_schema(
            "WithStack",
            vec![FieldDef::new("callchain", FieldType::StackFrames)],
        )
        .unwrap();
    enc.write_event(
        &schema,
        &[
            FieldValue::Varint(6_000),
            FieldValue::StackFrames(StackFrames::from(vec![1u64, 2, 3])),
        ],
    )
    .unwrap();
    let bytes = enc.finish();

    let event: WithStack = decode_first(&bytes);
    assert_eq!(event.timestamp_ns, 6_000);
    assert_eq!(event.callchain, vec![1, 2, 3]);
}

// ── Test 4b: Bytes field round-trip ─────────────────────────────────────────

#[derive(Debug, Deserialize, PartialEq)]
struct WithBytes {
    timestamp_ns: u64,
    payload: Vec<u8>,
}

#[test]
fn bytes_field_round_trip() {
    let mut enc = Encoder::new();
    let schema = enc
        .register_schema(
            "WithBytes",
            vec![FieldDef::new("payload", FieldType::Bytes)],
        )
        .unwrap();
    enc.write_event(
        &schema,
        &[
            FieldValue::Varint(6_500),
            FieldValue::Bytes(vec![0xDE, 0xAD, 0xBE, 0xEF]),
        ],
    )
    .unwrap();
    let bytes = enc.finish();

    let event: WithBytes = decode_first(&bytes);
    assert_eq!(event.timestamp_ns, 6_500);
    assert_eq!(event.payload, vec![0xDE, 0xAD, 0xBE, 0xEF]);
}

// ── Test 5: optional present and absent ─────────────────────────────────────

#[derive(Debug, Deserialize, PartialEq)]
struct WithOption {
    timestamp_ns: u64,
    name: Option<String>,
    count: Option<u64>,
}

#[test]
fn optional_fields_present() {
    let mut enc = Encoder::new();
    let pooled = enc.intern_string("present").unwrap();
    let schema = enc
        .register_schema(
            "WithOption",
            vec![
                FieldDef::new("name", FieldType::OptionalPooledString),
                FieldDef::new("count", FieldType::OptionalVarint),
            ],
        )
        .unwrap();
    enc.write_event(
        &schema,
        &[
            FieldValue::Varint(7_000),
            FieldValue::PooledString(pooled),
            FieldValue::Varint(99),
        ],
    )
    .unwrap();
    let bytes = enc.finish();

    let event: WithOption = decode_first(&bytes);
    assert_eq!(event.timestamp_ns, 7_000);
    assert_eq!(event.name.as_deref(), Some("present"));
    assert_eq!(event.count, Some(99));
}

#[test]
fn optional_fields_absent() {
    let mut enc = Encoder::new();
    let schema = enc
        .register_schema(
            "WithOption",
            vec![
                FieldDef::new("name", FieldType::OptionalPooledString),
                FieldDef::new("count", FieldType::OptionalVarint),
            ],
        )
        .unwrap();
    enc.write_event(
        &schema,
        &[
            FieldValue::Varint(8_000),
            FieldValue::None,
            FieldValue::None,
        ],
    )
    .unwrap();
    let bytes = enc.finish();

    let event: WithOption = decode_first(&bytes);
    assert_eq!(event.timestamp_ns, 8_000);
    assert_eq!(event.name, None);
    assert_eq!(event.count, None);
}

// ── Test 6: full type matrix using #[derive(TraceEvent)] ─────────────────────

#[derive(TraceEvent)]
#[allow(dead_code)]
struct KitchenSinkEvent {
    #[traceevent(timestamp)]
    timestamp_ns: u64,
    a_u8: u8,
    b_u16: u16,
    c_u32: u32,
    d_u64: u64,
    e_i64: i64,
    f_f64: f64,
    g_bool: bool,
    h_string: String,
    i_pooled: InternedString,
    j_frames: StackFrames,
}

#[derive(Debug, Deserialize, PartialEq)]
struct KitchenSinkDecoded {
    timestamp_ns: u64,
    a_u8: u64, // U8 wire type decodes as u64 / Varint
    b_u16: u64,
    c_u32: u64,
    d_u64: u64,
    e_i64: i64,
    f_f64: f64,
    g_bool: bool,
    h_string: String,
    i_pooled: String, // PooledString resolves transparently
    j_frames: Vec<u64>,
}

#[test]
fn full_type_matrix_round_trip() {
    let mut enc = Encoder::new();
    let interned = enc.intern_string("kitchen_pool_value").unwrap();
    let ev = KitchenSinkEvent {
        timestamp_ns: 9_000,
        a_u8: 250,
        b_u16: 60_000,
        c_u32: 0xDEAD_BEEF,
        d_u64: u64::MAX,
        e_i64: i64::MIN,
        f_f64: std::f64::consts::PI,
        g_bool: true,
        h_string: "an inline string".into(),
        i_pooled: interned,
        j_frames: StackFrames::from(vec![10, 20, 30]),
    };
    enc.write::<KitchenSinkEvent>(&ev).unwrap();
    let bytes = enc.finish();

    let decoded: KitchenSinkDecoded = decode_first(&bytes);
    assert_eq!(
        decoded,
        KitchenSinkDecoded {
            timestamp_ns: 9_000,
            a_u8: 250,
            b_u16: 60_000,
            c_u32: 0xDEAD_BEEF,
            d_u64: u64::MAX,
            e_i64: i64::MIN,
            f_f64: std::f64::consts::PI,
            g_bool: true,
            h_string: "an inline string".into(),
            i_pooled: "kitchen_pool_value".into(),
            j_frames: vec![10, 20, 30],
        }
    );
}

// ── Test 7: #[serde(other)] catches unknown events ─────────────────────────

#[derive(Debug, Deserialize, PartialEq)]
#[serde(tag = "event")]
enum OnlyKnown {
    Simple(Simple),
    #[serde(other)]
    Unknown,
}

#[test]
fn serde_other_catches_unknown_events() {
    let mut enc = Encoder::new();
    let known = enc
        .register_schema(
            "Simple",
            vec![
                FieldDef::new("worker_id", FieldType::Varint),
                FieldDef::new("task_id", FieldType::Varint),
            ],
        )
        .unwrap();
    let unknown = enc
        .register_schema(
            "TotallyUnknownEvent",
            vec![FieldDef::new("data", FieldType::Varint)],
        )
        .unwrap();

    enc.write_event(
        &known,
        &[
            FieldValue::Varint(10_000),
            FieldValue::Varint(0),
            FieldValue::Varint(1),
        ],
    )
    .unwrap();
    enc.write_event(
        &unknown,
        &[FieldValue::Varint(11_000), FieldValue::Varint(99)],
    )
    .unwrap();
    let bytes = enc.finish();

    let events: Vec<OnlyKnown> = decode_all(&bytes);
    assert_eq!(
        events,
        vec![
            OnlyKnown::Simple(Simple {
                timestamp_ns: 10_000,
                worker_id: 0,
                task_id: 1,
            }),
            OnlyKnown::Unknown,
        ]
    );
}

// ── Test 8: missing required field returns an error ─────────────────────────

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct ExpectsExtra {
    timestamp_ns: u64,
    worker_id: u64,
    /// Schema does NOT contain this field — deserialization must fail.
    task_id: u64,
    extra_required_field: u64,
}

#[test]
fn missing_required_field_errors() {
    // Schema has only worker_id + task_id. Struct expects an extra required
    // field — serde should report a missing-field error.
    let mut enc = Encoder::new();
    let schema = enc
        .register_schema(
            "Simple",
            vec![
                FieldDef::new("worker_id", FieldType::Varint),
                FieldDef::new("task_id", FieldType::Varint),
            ],
        )
        .unwrap();
    enc.write_event(
        &schema,
        &[
            FieldValue::Varint(12_000),
            FieldValue::Varint(0),
            FieldValue::Varint(1),
        ],
    )
    .unwrap();
    let bytes = enc.finish();

    let mut dec = Decoder::new(&bytes).expect("valid header");
    let mut got_err = false;
    dec.for_each_event(|raw| {
        let result = raw.deserialize::<ExpectsExtra>();
        if result.is_err() {
            got_err = true;
        }
    })
    .expect("no decode error");

    assert!(
        got_err,
        "deserializing into a struct with an extra required field should error"
    );
}

// ── Test 9: #[derive(TraceEvent)] + #[derive(Deserialize)] enum end-to-end ──

#[derive(TraceEvent)]
#[allow(dead_code)]
struct AppEvent {
    #[traceevent(timestamp)]
    timestamp_ns: u64,
    user_id: u64,
    action: InternedString,
}

#[derive(Debug, Deserialize, PartialEq)]
#[serde(tag = "event")]
enum AnalysisEvent {
    AppEvent {
        timestamp_ns: u64,
        user_id: u64,
        action: String,
    },
    #[serde(other)]
    Other,
}

#[test]
fn derive_traceevent_to_serde_enum_end_to_end() {
    let mut enc = Encoder::new();
    let click = enc.intern_string("click").unwrap();
    let scroll = enc.intern_string("scroll").unwrap();

    enc.write::<AppEvent>(&AppEvent {
        timestamp_ns: 100,
        user_id: 1,
        action: click,
    })
    .unwrap();
    enc.write::<AppEvent>(&AppEvent {
        timestamp_ns: 200,
        user_id: 2,
        action: scroll,
    })
    .unwrap();
    let bytes = enc.finish();

    let events: Vec<AnalysisEvent> = decode_all(&bytes);
    assert_eq!(
        events,
        vec![
            AnalysisEvent::AppEvent {
                timestamp_ns: 100,
                user_id: 1,
                action: "click".into(),
            },
            AnalysisEvent::AppEvent {
                timestamp_ns: 200,
                user_id: 2,
                action: "scroll".into(),
            },
        ]
    );
}

// ── Test 10: float and bool round-trip through deserializer ─────────────────

#[derive(Debug, Deserialize, PartialEq)]
struct Mixed {
    timestamp_ns: u64,
    pi: f64,
    flag: bool,
    delta: i64,
}

#[test]
fn float_bool_signed_round_trip() {
    let mut enc = Encoder::new();
    let schema = enc
        .register_schema(
            "Mixed",
            vec![
                FieldDef::new("pi", FieldType::F64),
                FieldDef::new("flag", FieldType::Bool),
                FieldDef::new("delta", FieldType::I64),
            ],
        )
        .unwrap();
    enc.write_event(
        &schema,
        &[
            FieldValue::Varint(13_000),
            FieldValue::F64(std::f64::consts::PI),
            FieldValue::Bool(true),
            FieldValue::I64(-9_999),
        ],
    )
    .unwrap();
    let bytes = enc.finish();

    let event: Mixed = decode_first(&bytes);
    assert_eq!(
        event,
        Mixed {
            timestamp_ns: 13_000,
            pi: std::f64::consts::PI,
            flag: true,
            delta: -9_999,
        }
    );
}

// ── Test 11: Option<T> field entirely absent from wire schema ────────────────

#[derive(Debug, Deserialize, PartialEq)]
struct NewerStruct {
    timestamp_ns: u64,
    worker_id: u64,
    /// This field doesn't exist in the wire schema — should default to None.
    new_optional_field: Option<String>,
}

#[test]
fn option_field_absent_from_schema_defaults_to_none() {
    // Schema only has "worker_id" — no "new_optional_field". This simulates
    // reading an old trace with a newer decoder struct that added an Option field.
    let mut enc = Encoder::new();
    let schema = enc
        .register_schema(
            "NewerStruct",
            vec![FieldDef::new("worker_id", FieldType::Varint)],
        )
        .unwrap();
    enc.write_event(
        &schema,
        &[FieldValue::Varint(1_000), FieldValue::Varint(42)],
    )
    .unwrap();
    let bytes = enc.finish();

    let event: NewerStruct = decode_first(&bytes);
    assert_eq!(event.timestamp_ns, 1_000);
    assert_eq!(event.worker_id, 42);
    assert_eq!(event.new_optional_field, None);
}

// ── Test 11b: pool-miss error paths ──────────────────────────────────────────

#[test]
fn pooled_string_miss_returns_error() {
    let mut enc = Encoder::new();
    let schema = enc
        .register_schema(
            "WithPool",
            vec![FieldDef::new("label", FieldType::PooledString)],
        )
        .unwrap();
    // Write an event referencing pool ID 999 which was never interned.
    enc.write_event(
        &schema,
        &[
            FieldValue::Varint(1_000),
            FieldValue::PooledString(InternedString::from_raw(999)),
        ],
    )
    .unwrap();
    let bytes = enc.finish();

    let mut dec = Decoder::new(&bytes).expect("valid header");
    let mut got_err = false;
    dec.for_each_event(|raw| {
        if let Err(e) = raw.deserialize::<WithPool>() {
            assert!(
                e.message().contains("not found in string pool"),
                "unexpected error: {e}"
            );
            got_err = true;
        }
    })
    .expect("no decode error");
    assert!(got_err, "expected a pool-miss error");
}

#[test]
fn pooled_stack_frames_miss_returns_error() {
    let mut enc = Encoder::new();
    let schema = enc
        .register_schema(
            "WithStack",
            vec![FieldDef::new("callchain", FieldType::PooledStackFrames)],
        )
        .unwrap();
    enc.write_event(
        &schema,
        &[
            FieldValue::Varint(2_000),
            FieldValue::PooledStackFrames(InternedStackFrames::from_raw(999)),
        ],
    )
    .unwrap();
    let bytes = enc.finish();

    let mut dec = Decoder::new(&bytes).expect("valid header");
    let mut got_err = false;
    dec.for_each_event(|raw| {
        if let Err(e) = raw.deserialize::<WithStack>() {
            assert!(
                e.message().contains("not found in stack pool"),
                "unexpected error: {e}"
            );
            got_err = true;
        }
    })
    .expect("no decode error");
    assert!(got_err, "expected a pool-miss error");
}

// ── Test 11c: type coercion failure (bool from varint) ───────────────────────

#[derive(Debug, Deserialize, PartialEq)]
struct WantsBool {
    timestamp_ns: u64,
    flag: bool,
}

#[test]
fn type_coercion_bool_from_varint_returns_error() {
    let mut enc = Encoder::new();
    let schema = enc
        .register_schema("WantsBool", vec![FieldDef::new("flag", FieldType::Varint)])
        .unwrap();
    // Wire has a varint where the struct expects a bool.
    enc.write_event(
        &schema,
        &[FieldValue::Varint(1_000), FieldValue::Varint(42)],
    )
    .unwrap();
    let bytes = enc.finish();

    let mut dec = Decoder::new(&bytes).expect("valid header");
    let mut got_err = false;
    dec.for_each_event(|raw| {
        if let Err(e) = raw.deserialize::<WantsBool>() {
            assert!(
                e.message().contains("invalid type"),
                "unexpected error: {e}"
            );
            got_err = true;
        }
    })
    .expect("no decode error");
    assert!(got_err, "expected a type coercion error");
}

// ── Test 12: DynamicList deserializes into Vec<T> ───────────────────────────

#[derive(Debug, Deserialize, PartialEq)]
struct WithList {
    timestamp_ns: u64,
    items: Vec<String>,
}

#[test]
fn dynamic_list_deserializes_to_vec() {
    let mut enc = Encoder::new();
    let schema = enc
        .register_schema(
            "WithList",
            vec![FieldDef::new("items", FieldType::DynamicList)],
        )
        .unwrap();
    enc.write_event(
        &schema,
        &[
            FieldValue::Varint(1_000),
            FieldValue::List(vec![
                FieldValue::String("hello".into()),
                FieldValue::String("world".into()),
            ]),
        ],
    )
    .unwrap();
    let bytes = enc.finish();

    let event: WithList = decode_first(&bytes);
    assert_eq!(event.timestamp_ns, 1_000);
    assert_eq!(event.items, vec!["hello", "world"]);
}

// ── Test 13: StringMap deserializes into HashMap<String, String> ─────────────

use std::collections::HashMap;

#[derive(Debug, Deserialize, PartialEq)]
struct WithStringMap {
    timestamp_ns: u64,
    headers: HashMap<String, String>,
}

#[test]
fn string_map_deserializes_to_hashmap() {
    let mut enc = Encoder::new();
    let schema = enc
        .register_schema(
            "WithStringMap",
            vec![FieldDef::new("headers", FieldType::StringMap)],
        )
        .unwrap();
    enc.write_event(
        &schema,
        &[
            FieldValue::Varint(2_000),
            FieldValue::StringMap(vec![
                ("content-type".into(), "application/json".into()),
                ("x-request-id".into(), "abc123".into()),
            ]),
        ],
    )
    .unwrap();
    let bytes = enc.finish();

    let event: WithStringMap = decode_first(&bytes);
    assert_eq!(event.timestamp_ns, 2_000);
    let mut expected = HashMap::new();
    expected.insert("content-type".into(), "application/json".into());
    expected.insert("x-request-id".into(), "abc123".into());
    assert_eq!(event.headers, expected);
}

// ── Test 14: DynamicMap deserializes into HashMap<String, u64> ───────────────

#[derive(Debug, Deserialize, PartialEq)]
struct WithDynamicMap {
    timestamp_ns: u64,
    counts: HashMap<String, u64>,
}

#[test]
fn dynamic_map_deserializes_to_hashmap() {
    let mut enc = Encoder::new();
    let schema = enc
        .register_schema(
            "WithDynamicMap",
            vec![FieldDef::new("counts", FieldType::DynamicMap)],
        )
        .unwrap();
    enc.write_event(
        &schema,
        &[
            FieldValue::Varint(3_000),
            FieldValue::Map(vec![
                (FieldValue::String("reads".into()), FieldValue::Varint(100)),
                (FieldValue::String("writes".into()), FieldValue::Varint(42)),
            ]),
        ],
    )
    .unwrap();
    let bytes = enc.finish();

    let event: WithDynamicMap = decode_first(&bytes);
    assert_eq!(event.timestamp_ns, 3_000);
    let mut expected = HashMap::new();
    expected.insert("reads".into(), 100u64);
    expected.insert("writes".into(), 42u64);
    assert_eq!(event.counts, expected);
}

// ── Test 15: DynamicMap deserializes into a typed struct ─────────────────────

#[derive(Debug, Deserialize, PartialEq)]
struct Metadata {
    status: u64,
    path: String,
}

#[derive(Debug, Deserialize, PartialEq)]
struct WithStructMap {
    timestamp_ns: u64,
    meta: Metadata,
}

#[test]
fn dynamic_map_deserializes_to_struct() {
    let mut enc = Encoder::new();
    let schema = enc
        .register_schema(
            "WithStructMap",
            vec![FieldDef::new("meta", FieldType::DynamicMap)],
        )
        .unwrap();
    enc.write_event(
        &schema,
        &[
            FieldValue::Varint(4_000),
            FieldValue::Map(vec![
                (FieldValue::String("status".into()), FieldValue::Varint(200)),
                (
                    FieldValue::String("path".into()),
                    FieldValue::String("/api/users".into()),
                ),
            ]),
        ],
    )
    .unwrap();
    let bytes = enc.finish();

    let event: WithStructMap = decode_first(&bytes);
    assert_eq!(event.timestamp_ns, 4_000);
    assert_eq!(
        event.meta,
        Metadata {
            status: 200,
            path: "/api/users".into(),
        }
    );
}

// ── Test 16: StringMap deserializes into Vec<(String, String)> ─────────────

/// `Vec<(String, String)>` is the natural Rust shape for the wire-format
/// `StringMap` when callers want order-preserving entries instead of a
/// `HashMap`. The deserializer should present a `StringMap` as a sequence of
/// (key, value) pairs so that this just works without `#[serde(deserialize_with = "...")]`
/// helpers.
#[derive(Debug, Deserialize, PartialEq)]
struct WithStringMapAsVec {
    timestamp_ns: u64,
    headers: Vec<(String, String)>,
}

#[test]
fn string_map_deserializes_to_vec_of_tuples() {
    let mut enc = Encoder::new();
    let schema = enc
        .register_schema(
            "WithStringMapAsVec",
            vec![FieldDef::new("headers", FieldType::StringMap)],
        )
        .unwrap();
    enc.write_event(
        &schema,
        &[
            FieldValue::Varint(5_000),
            FieldValue::StringMap(vec![
                ("content-type".into(), "application/json".into()),
                ("x-request-id".into(), "abc123".into()),
            ]),
        ],
    )
    .unwrap();
    let bytes = enc.finish();

    let event: WithStringMapAsVec = decode_first(&bytes);
    assert_eq!(event.timestamp_ns, 5_000);
    assert_eq!(
        event.headers,
        vec![
            ("content-type".to_string(), "application/json".to_string()),
            ("x-request-id".to_string(), "abc123".to_string()),
        ]
    );
}
