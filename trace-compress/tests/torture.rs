//! Hand-built synthetic traces exercising wire-format corners the demo trace
//! doesn't hit. Every test requires a byte-exact round trip.

use dial9_trace_compress::compress::{compress, decompress};

// Field type tags (mirrors dial9-trace-format).
const I64: u8 = 1;
const F64: u8 = 2;
const BOOL: u8 = 3;
const STRING: u8 = 4;
const BYTES: u8 = 5;
const POOLED_STACK: u8 = 6;
const POOLED_STRING: u8 = 7;
const STACK_FRAMES: u8 = 8;
const VARINT: u8 = 9;
const STRING_MAP: u8 = 10;
const U8: u8 = 11;
const U16: u8 = 12;
const U32: u8 = 13;
const DYN_LIST: u8 = 14;
const DYN_MAP: u8 = 15;
const OPT: u8 = 0x80;

struct TraceBuilder {
    buf: Vec<u8>,
}

impl TraceBuilder {
    fn new() -> Self {
        let mut buf = Vec::new();
        buf.extend_from_slice(&[0x54, 0x52, 0x43, 0x00, 1]); // TRC\0 v1
        TraceBuilder { buf }
    }

    /// Start a new segment (mid-stream reset header).
    fn segment(&mut self) {
        self.buf.extend_from_slice(&[0x54, 0x52, 0x43, 0x00, 1]);
    }

    fn schema(&mut self, type_id: u16, name: &str, has_ts: bool, fields: &[(&str, u8)]) {
        self.buf.push(0x01);
        self.buf.extend_from_slice(&type_id.to_le_bytes());
        self.buf
            .extend_from_slice(&(name.len() as u16).to_le_bytes());
        self.buf.extend_from_slice(name.as_bytes());
        self.buf.push(has_ts as u8);
        self.buf
            .extend_from_slice(&(fields.len() as u16).to_le_bytes());
        for (fname, tag) in fields {
            self.buf
                .extend_from_slice(&(fname.len() as u16).to_le_bytes());
            self.buf.extend_from_slice(fname.as_bytes());
            self.buf.push(*tag);
        }
    }

    /// Event header; caller appends field payloads via the `v_*` helpers.
    fn event(&mut self, type_id: u16, ts_delta: Option<u32>) {
        self.buf.push(0x02);
        self.buf.extend_from_slice(&type_id.to_le_bytes());
        if let Some(d) = ts_delta {
            self.buf
                .extend_from_slice(&[d as u8, (d >> 8) as u8, (d >> 16) as u8]);
        }
    }

    fn ts_reset(&mut self, ts: u64) {
        self.buf.push(0x05);
        self.buf.extend_from_slice(&ts.to_le_bytes());
    }

    fn string_pool(&mut self, entries: &[(u32, &[u8])]) {
        self.buf.push(0x03);
        self.buf
            .extend_from_slice(&(entries.len() as u32).to_le_bytes());
        for (id, data) in entries {
            self.buf.extend_from_slice(&id.to_le_bytes());
            self.buf
                .extend_from_slice(&(data.len() as u32).to_le_bytes());
            self.buf.extend_from_slice(data);
        }
    }

    fn stack_pool(&mut self, entries: &[(u32, &[u64])]) {
        self.buf.push(0x04);
        self.buf
            .extend_from_slice(&(entries.len() as u32).to_le_bytes());
        for (id, frames) in entries {
            self.buf.extend_from_slice(&id.to_le_bytes());
            self.buf
                .extend_from_slice(&(frames.len() as u32).to_le_bytes());
            for f in *frames {
                self.buf.extend_from_slice(&f.to_le_bytes());
            }
        }
    }

    fn annotations(&mut self, type_id: u16, entries: &[(u16, &str, &str)]) {
        self.buf.push(0x06);
        // type id is LEB128 in annotation frames
        self.v_varint(type_id as u64);
        self.buf
            .extend_from_slice(&(entries.len() as u16).to_le_bytes());
        for (idx, k, v) in entries {
            self.buf.extend_from_slice(&idx.to_le_bytes());
            self.buf.extend_from_slice(&(k.len() as u16).to_le_bytes());
            self.buf.extend_from_slice(k.as_bytes());
            self.buf.extend_from_slice(&(v.len() as u32).to_le_bytes());
            self.buf.extend_from_slice(v.as_bytes());
        }
    }

    // --- field value payloads ---

    fn v_varint(&mut self, mut v: u64) {
        while v >= 0x80 {
            self.buf.push((v as u8) | 0x80);
            v >>= 7;
        }
        self.buf.push(v as u8);
    }

    fn v_i64(&mut self, v: i64) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    fn v_f64(&mut self, v: f64) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    fn v_blob(&mut self, b: &[u8]) {
        self.buf.extend_from_slice(&(b.len() as u32).to_le_bytes());
        self.buf.extend_from_slice(b);
    }

    fn v_u32(&mut self, v: u32) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    fn v_stack(&mut self, frames: &[u64]) {
        self.buf
            .extend_from_slice(&(frames.len() as u32).to_le_bytes());
        for f in frames {
            self.buf.extend_from_slice(&f.to_le_bytes());
        }
    }

    fn v_string_map(&mut self, pairs: &[(&[u8], &[u8])]) {
        self.buf
            .extend_from_slice(&(pairs.len() as u32).to_le_bytes());
        for (k, v) in pairs {
            self.buf.extend_from_slice(&(k.len() as u32).to_le_bytes());
            self.buf.extend_from_slice(k);
            self.buf.extend_from_slice(&(v.len() as u32).to_le_bytes());
            self.buf.extend_from_slice(v);
        }
    }

    /// Presence prefix for an optional field. The decoder treats any nonzero
    /// byte as "present", so nonstandard prefixes must survive round trips.
    fn presence(&mut self, prefix: u8) {
        self.buf.push(prefix);
    }
}

fn round_trip(raw: &[u8]) -> Vec<u8> {
    let c = compress(raw, 3).expect("compress");
    let restored = decompress(&c).expect("decompress");
    assert!(
        raw == restored.as_slice(),
        "round trip not byte-exact: {} in, {} out",
        raw.len(),
        restored.len()
    );
    c
}

#[test]
fn kitchen_sink_all_field_types() {
    let mut t = TraceBuilder::new();
    t.schema(
        1,
        "Kitchen",
        true,
        &[
            ("a_i64", I64),
            ("a_f64", F64),
            ("a_bool", BOOL),
            ("a_string", STRING),
            ("a_bytes", BYTES),
            ("a_pstack", POOLED_STACK),
            ("a_pstring", POOLED_STRING),
            ("a_stack", STACK_FRAMES),
            ("a_varint", VARINT),
            ("a_map", STRING_MAP),
            ("a_u8", U8),
            ("a_u16", U16),
            ("a_u32", U32),
            ("a_list", DYN_LIST),
            ("a_dynmap", DYN_MAP),
        ],
    );
    t.string_pool(&[(0, b"hello"), (1, b""), (2, &[0xff, 0xfe, 0x00])]);
    t.stack_pool(&[(0, &[]), (1, &[u64::MAX]), (2, &[0, 5, u64::MAX, 1])]);
    for i in 0..50u64 {
        t.event(1, Some((i * 3) as u32));
        t.v_i64(-(i as i64) * 1_000_000_007);
        t.v_f64(i as f64 * std::f64::consts::PI);
        t.buf.push((i % 2) as u8);
        t.v_blob(format!("s{i}").as_bytes());
        t.v_blob(&[i as u8; 3]);
        t.v_u32((i % 3) as u32); // pooled stack id
        t.v_u32((i % 3) as u32); // pooled string id
        t.v_stack(&[i, i.wrapping_mul(0x9e3779b97f4a7c15), 42]);
        t.v_varint(u64::MAX - i);
        t.v_string_map(&[(b"k" as &[u8], b"v" as &[u8]), (b"", b"")]);
        t.buf.push(i as u8);
        t.buf.extend_from_slice(&(i as u16 * 999).to_le_bytes());
        t.v_u32(i as u32 * 77777);
        // dynamic list: [varint 7, string "x", nested list [bool true]]
        t.v_u32(3);
        t.buf.push(VARINT);
        t.v_varint(7);
        t.buf.push(STRING);
        t.v_blob(b"x");
        t.buf.push(DYN_LIST);
        t.v_u32(1);
        t.buf.push(BOOL);
        t.buf.push(1);
        // dynamic map: {varint 1: f64 2.5}
        t.v_u32(1);
        t.buf.push(VARINT);
        t.v_varint(1);
        t.buf.push(F64);
        t.v_f64(2.5);
    }
    round_trip(&t.buf);
}

#[test]
fn optionals_including_nonstandard_presence() {
    let mut t = TraceBuilder::new();
    t.schema(
        7,
        "Opt",
        true,
        &[
            ("o_varint", VARINT | OPT),
            ("o_string", STRING | OPT),
            ("o_i64", I64 | OPT),
        ],
    );
    for i in 0..20u32 {
        t.event(7, Some(i));
        match i % 3 {
            0 => {
                t.presence(0); // absent
                t.presence(1);
                t.v_blob(b"present");
                t.presence(0);
            }
            1 => {
                t.presence(1);
                t.v_varint(1 << 40);
                t.presence(0);
                // Nonstandard nonzero presence byte must survive byte-exact.
                t.presence(0x2a);
                t.v_i64(-1);
            }
            _ => {
                t.presence(1);
                t.v_varint(3);
                t.presence(1);
                t.v_blob(b"");
                t.presence(1);
                t.v_i64(i64::MIN);
            }
        }
    }
    round_trip(&t.buf);
}

#[test]
fn type_id_remapped_across_segments() {
    // The same wire type id maps to *different* schemas in different
    // segments (dynamic ids are assigned per segment). Column state must not
    // bleed between them.
    let mut t = TraceBuilder::new();
    t.schema(300, "First", true, &[("value", VARINT)]);
    t.event(300, Some(10));
    t.v_varint(123456789);

    t.segment();
    // Same id, now a string field — and no timestamp.
    t.schema(300, "Second", false, &[("value", STRING)]);
    t.event(300, None);
    t.v_blob(b"same id, different schema");

    t.segment();
    // Same id, same definition as segment 1 (same schema group again).
    t.schema(300, "First", true, &[("value", VARINT)]);
    t.event(300, Some(11));
    t.v_varint(987654321);

    round_trip(&t.buf);
}

#[test]
fn span_id_cache_and_lookalikes() {
    let mut t = TraceBuilder::new();
    t.schema(
        1,
        "Enter",
        true,
        &[("span_id", VARINT), ("parent_span_id", VARINT | OPT)],
    );
    t.schema(2, "Exit", true, &[("span_id", VARINT)]);
    // A field *named* span_id that is not a varint must not go through the
    // id cache.
    t.schema(3, "Lookalike", true, &[("span_id", STRING)]);
    let ids = [0u64, u64::MAX, 0xdead_beef_cafe, 1, 0xdead_beef_cafe];
    for (i, &id) in ids.iter().enumerate() {
        t.event(1, Some(i as u32));
        t.v_varint(id);
        if i % 2 == 0 {
            t.presence(0);
        } else {
            t.presence(1);
            t.v_varint(ids[i - 1]); // parent = previous span (cache hit)
        }
        t.event(2, Some(i as u32));
        t.v_varint(id); // exit same span (cache hit)
        t.event(3, Some(i as u32));
        t.v_blob(format!("span_id_str_{id}").as_bytes());
    }
    round_trip(&t.buf);
}

#[test]
fn pools_and_resets_edge_cases() {
    let mut t = TraceBuilder::new();
    t.schema(1, "E", true, &[("v", VARINT)]);
    // Non-monotonic and duplicate pool ids.
    t.string_pool(&[(5, b"z"), (0, b"a"), (5, b"z2"), (u32::MAX, b"wrap")]);
    // UUID variants: lowercase (packed), uppercase / wrong shape (literal).
    t.string_pool(&[
        (10, b"74f5d739-7397-489f-9fb7-6f9aa3986df3"),
        (11, b"74F5D739-7397-489F-9FB7-6F9AA3986DF3"),
        (12, b"74f5d739x7397-489f-9fb7-6f9aa3986df3"),
        (13, b"74f5d739-7397-489f-9fb7-6f9aa3986df"),
    ]);
    t.string_pool(&[]); // empty pool frame
    t.stack_pool(&[]);
    t.stack_pool(&[(0, &[1, 2]), (1, &[u64::MAX, 0, u64::MAX])]);
    // Timestamp resets: forward jump, backward jump, zero.
    t.ts_reset(1 << 60);
    t.event(1, Some(0));
    t.v_varint(1);
    t.ts_reset(5);
    t.event(1, Some(0xFF_FFFF)); // max u24 delta
    t.v_varint(2);
    t.ts_reset(0);
    t.event(1, Some(0));
    t.v_varint(3);
    t.annotations(1, &[(0, "metrique.unit", "nanoseconds"), (0, "k", "")]);
    round_trip(&t.buf);
}

#[test]
fn compresses_smaller_than_input_on_repetitive_trace() {
    // Sanity: a plausibly-shaped trace should actually shrink.
    let mut t = TraceBuilder::new();
    t.schema(1, "Poll", true, &[("worker", VARINT), ("task", VARINT)]);
    for i in 0..5000u64 {
        t.event(1, Some((i % 900) as u32 * 100));
        t.v_varint(i % 4);
        t.v_varint(i % 37);
    }
    let c = round_trip(&t.buf);
    assert!(
        c.len() * 4 < t.buf.len(),
        "expected >4x on repetitive trace, got {} -> {}",
        t.buf.len(),
        c.len()
    );
}
